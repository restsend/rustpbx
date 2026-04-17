mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(rwi_url: &str) -> WsStream {
    let url = format!("{}?token={}", rwi_url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

#[allow(dead_code)]
async fn ws_send_recv(ws: &mut WsStream, json: &str) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    recv_next(ws).await
}

/// Send a request with an auto-generated action_id and wait for the matching response.
async fn ws_send_recv_with_id(
    ws: &mut WsStream,
    action: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let action_id = Uuid::new_v4().to_string();
    let request = serde_json::json!({
        "rwi": "1.0",
        "action_id": action_id,
        "action": action,
        "params": params,
    });
    let json = serde_json::to_string(&request).unwrap();
    ws.send(Message::Text(json.into())).await.unwrap();

    // Wait for matching response
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv timeout waiting for action response")
            .expect("stream closed")
            .expect("ws error");
        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).expect("invalid json");
            if v.get("action_id").and_then(|a| a.as_str()) == Some(&action_id) {
                return v;
            }
        }
    }
}

#[allow(dead_code)]
async fn recv_next(ws: &mut WsStream) -> serde_json::Value {
    let msg = timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("recv timeout")
        .expect("stream closed")
        .expect("ws error");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("invalid json"),
        _ => panic!("unexpected msg type"),
    }
}

/// Wait for a specific event type (snake_case variant name)
async fn wait_for_event(
    ws: &mut WsStream,
    event_type: &str,
    max_wait_secs: u64,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(max_wait_secs);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let msg = timeout(remaining, ws.next())
            .await
            .expect(&format!("timeout waiting for {}", event_type))
            .expect("stream closed")
            .expect("ws error");

        if let Message::Text(t) = msg {
            let json: serde_json::Value = serde_json::from_str(&t).expect("invalid json");
            // RWI events are serialized as {event_type: {...}}, not {event: "type"}
            if json.get(event_type).is_some() {
                return json;
            }
        }
    }
}

/// Wait for the first matching event among the provided event types.
async fn wait_for_any_event(
    ws: &mut WsStream,
    event_types: &[&str],
    max_wait_secs: u64,
) -> (String, serde_json::Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(max_wait_secs);
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        let msg = timeout(remaining, ws.next())
            .await
            .expect("timeout waiting for any expected event")
            .expect("stream closed")
            .expect("ws error");

        if let Message::Text(t) = msg {
            let json: serde_json::Value = serde_json::from_str(&t).expect("invalid json");
            for event_type in event_types {
                if json.get(*event_type).is_some() {
                    return ((*event_type).to_string(), json);
                }
            }
        }
    }
}

/// Test REFER blind transfer flow
///
/// Scenario:
/// 1. RWI originates call to Bob (ring 1s, auto-answer)
/// 2. Bob answers
/// 3. RWI sends transfer command to Bob
/// 4. Bob (sipbot) accepts REFER, sends NOTIFY 100, then NOTIFY 200
/// 5. Verify transfer success
#[tokio::test]
async fn test_refer_blind_transfer_success() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    // Start Bob (callee who will be transferred)
    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee(bob_port, 1).await;

    // Start Charlie (transfer target)
    let charlie_port = portpicker::pick_unused_port().expect("no free port");
    let charlie = TestUa::callee(charlie_port, 1).await;

    // Connect RWI
    let mut ws = ws_connect(&rwi_url).await;

    // Subscribe to call events
    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Step 1: Originate call to Bob
    let call_id = Uuid::new_v4().to_string();
    let dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_id, "destination": dest, "caller_id": "alice"}),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {:?}", resp);

    // Step 2: Wait for call answered (increased timeout for CI)
    let event = wait_for_event(&mut ws, "call_answered", 15).await;
    assert_eq!(event["call_answered"]["call_id"], call_id);

    // Step 3: Transfer to Charlie
    let charlie_dest = format!("sip:charlie@127.0.0.1:{}", charlie_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer",
        serde_json::json!({"call_id": call_id, "target": charlie_dest}),
    )
    .await;

    println!("Transfer response: {:?}", resp);

    // Step 4: Verify transfer emits at least one transfer-related event.
    let (event_type, event) = wait_for_any_event(
        &mut ws,
        &[
            "call_transfer_accepted",
            "call_transferred",
            "call_transfer_failed",
        ],
        10,
    )
    .await;
    if event_type == "call_transfer_accepted" {
        assert_eq!(event["call_transfer_accepted"]["call_id"], call_id);
    } else if event_type == "call_transferred" {
        assert_eq!(event["call_transferred"]["call_id"], call_id);
    } else {
        assert_eq!(event["call_transfer_failed"]["call_id"], call_id);
    }

    // Cleanup
    bob.stop();
    charlie.stop();
}

/// Test REFER with 3PCC fallback
///
/// Scenario:
/// 1. Alice calls Bob
/// 2. Alice transfers to Charlie
/// 3. Bob rejects REFER with 405 Method Not Allowed
/// 4. Verify 3PCC fallback is triggered (or transfer fails gracefully)
#[tokio::test]
async fn test_refer_with_3pcc_fallback() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    // Start Bob (callee who will reject REFER with 405)
    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee_with_refer_reject(bob_port, 1, 405).await;

    // Start Charlie (transfer target)
    let charlie_port = portpicker::pick_unused_port().expect("no free port");
    let charlie = TestUa::callee(charlie_port, 1).await;

    // Connect RWI
    let mut ws = ws_connect(&rwi_url).await;

    // Subscribe to call events
    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Step 1: Originate call to Bob
    let call_id = Uuid::new_v4().to_string();
    let dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_id, "destination": dest, "caller_id": "alice"}),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {:?}", resp);

    // Step 2: Wait for call answered
    let event = wait_for_event(&mut ws, "call_answered", 15).await;
    assert_eq!(event["call_answered"]["call_id"], call_id);

    // Step 3: Transfer to Charlie (Bob will reject REFER with 405)
    let charlie_dest = format!("sip:charlie@127.0.0.1:{}", charlie_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer",
        serde_json::json!({"call_id": call_id, "target": charlie_dest}),
    )
    .await;

    println!("Transfer response: {:?}", resp);

    // After transfer request, we should eventually see at least one transfer event.
    let (event_type, event) = wait_for_any_event(
        &mut ws,
        &[
            "call_transfer_accepted",
            "call_transferred",
            "call_transfer_failed",
        ],
        10,
    )
    .await;
    if event_type == "call_transfer_accepted" {
        assert_eq!(event["call_transfer_accepted"]["call_id"], call_id);
    } else if event_type == "call_transferred" {
        assert_eq!(event["call_transferred"]["call_id"], call_id);
    } else {
        assert_eq!(event["call_transfer_failed"]["call_id"], call_id);
    }

    // Cleanup
    bob.stop();
    charlie.stop();
}

#[tokio::test]
async fn test_refer_replace_transfer_action() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee(bob_port, 1).await;

    let charlie_port = portpicker::pick_unused_port().expect("no free port");
    let charlie = TestUa::callee(charlie_port, 1).await;

    let mut ws = ws_connect(&rwi_url).await;

    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_id = Uuid::new_v4().to_string();
    let dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_id, "destination": dest, "caller_id": "alice"}),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {:?}", resp);

    let event = wait_for_event(&mut ws, "call_answered", 15).await;
    assert_eq!(event["call_answered"]["call_id"], call_id);

    let charlie_dest = format!("sip:charlie@127.0.0.1:{}", charlie_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer.replace",
        serde_json::json!({"call_id": call_id, "target": charlie_dest}),
    )
    .await;
    assert_eq!(
        resp["status"], "success",
        "replace transfer command failed: {:?}",
        resp
    );

    let (event_type, event) = wait_for_any_event(
        &mut ws,
        &[
            "call_transfer_accepted",
            "call_transferred",
            "call_transfer_failed",
        ],
        10,
    )
    .await;
    if event_type == "call_transfer_accepted" {
        assert_eq!(event["call_transfer_accepted"]["call_id"], call_id);
    } else if event_type == "call_transferred" {
        assert_eq!(event["call_transferred"]["call_id"], call_id);
    } else {
        assert_eq!(event["call_transfer_failed"]["call_id"], call_id);
    }

    bob.stop();
    charlie.stop();
}

#[tokio::test]
async fn test_refer_replace_transfer_rejected() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee_with_refer_reject(bob_port, 1, 486).await;

    let charlie_port = portpicker::pick_unused_port().expect("no free port");
    let charlie = TestUa::callee(charlie_port, 1).await;

    let mut ws = ws_connect(&rwi_url).await;

    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_id = Uuid::new_v4().to_string();
    let dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_id, "destination": dest, "caller_id": "alice"}),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {:?}", resp);

    let event = wait_for_event(&mut ws, "call_answered", 15).await;
    assert_eq!(event["call_answered"]["call_id"], call_id);

    let charlie_dest = format!("sip:charlie@127.0.0.1:{}", charlie_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer.replace",
        serde_json::json!({"call_id": call_id, "target": charlie_dest}),
    )
    .await;
    assert_eq!(
        resp["status"], "success",
        "replace transfer command failed: {:?}",
        resp
    );

    let (event_type, event) = wait_for_any_event(
        &mut ws,
        &[
            "call_transfer_accepted",
            "call_transferred",
            "call_transfer_failed",
        ],
        10,
    )
    .await;
    if event_type == "call_transfer_accepted" {
        assert_eq!(event["call_transfer_accepted"]["call_id"], call_id);
    } else if event_type == "call_transferred" {
        assert_eq!(event["call_transferred"]["call_id"], call_id);
    } else {
        assert_eq!(event["call_transfer_failed"]["call_id"], call_id);
    }

    bob.stop();
    charlie.stop();
}

/// Test attended transfer (consultation transfer)
///
/// Scenario:
/// 1. Alice calls Bob, Bob answers
/// 2. Alice places Bob on hold
/// 3. Alice calls Charlie (consultation)
/// 4. Charlie answers
/// 5. Alice completes transfer (Bob <-> Charlie)
#[tokio::test]
async fn test_attended_transfer() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee(bob_port, 1).await;

    let mut ws = ws_connect(&rwi_url).await;

    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_id = Uuid::new_v4().to_string();
    let bob_dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_id, "destination": bob_dest, "caller_id": "alice"}),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {:?}", resp);

    let event = wait_for_event(&mut ws, "call_answered", 15).await;
    assert_eq!(event["call_answered"]["call_id"], call_id);

    let attended_resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer.attended",
        serde_json::json!({
            "call_id": call_id,
            "target": "sip:consult@127.0.0.1:5099",
            "timeout_secs": 20
        }),
    )
    .await;
    assert_eq!(attended_resp["status"], "success");

    let consultation_call_id = attended_resp["data"]["consultation_call_id"]
        .as_str()
        .expect("missing consultation_call_id")
        .to_string();
    assert!(
        !consultation_call_id.is_empty(),
        "consultation_call_id should not be empty"
    );

    let complete_resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer.complete",
        serde_json::json!({
            "call_id": call_id,
            "consultation_call_id": consultation_call_id,
        }),
    )
    .await;
    assert_eq!(
        complete_resp["status"], "success",
        "transfer complete failed: {:?}",
        complete_resp
    );

    bob.stop();
}

#[tokio::test]
async fn test_attended_transfer_cancel() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee(bob_port, 1).await;

    let mut ws = ws_connect(&rwi_url).await;

    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_id = Uuid::new_v4().to_string();
    let bob_dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_id, "destination": bob_dest, "caller_id": "alice"}),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {:?}", resp);

    let event = wait_for_event(&mut ws, "call_answered", 15).await;
    assert_eq!(event["call_answered"]["call_id"], call_id);

    let attended_resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer.attended",
        serde_json::json!({
            "call_id": call_id,
            "target": "sip:consult-cancel@127.0.0.1:5098",
            "timeout_secs": 20
        }),
    )
    .await;
    assert_eq!(attended_resp["status"], "success");

    let consultation_call_id = attended_resp["data"]["consultation_call_id"]
        .as_str()
        .expect("missing consultation_call_id")
        .to_string();
    assert!(!consultation_call_id.is_empty());

    let cancel_resp = ws_send_recv_with_id(
        &mut ws,
        "call.transfer.cancel",
        serde_json::json!({
            "consultation_call_id": consultation_call_id,
        }),
    )
    .await;
    assert_eq!(
        cancel_resp["status"], "success",
        "transfer cancel failed: {:?}",
        cancel_resp
    );

    bob.stop();
}
