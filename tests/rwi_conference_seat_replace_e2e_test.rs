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

async fn ws_send_recv_with_id(
    ws: &mut WsStream,
    action: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let (response, _) = ws_send_recv_with_id_and_events(ws, action, params).await;
    response
}

async fn ws_send_recv_with_id_and_events(
    ws: &mut WsStream,
    action: &str,
    params: serde_json::Value,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let action_id = Uuid::new_v4().to_string();
    let request = serde_json::json!({
        "rwi": "1.0",
        "action_id": action_id,
        "action": action,
        "params": params,
    });
    let json = serde_json::to_string(&request).unwrap();
    ws.send(Message::Text(json.into())).await.unwrap();

    let mut others = Vec::new();
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
                return (v, others);
            }
            others.push(v);
        }
    }
}

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
            if json.get(event_type).is_some() {
                return json;
            }
        }
    }
}

fn has_event(events: &[serde_json::Value], event_type: &str) -> bool {
    events.iter().any(|e| e.get(event_type).is_some())
}

#[tokio::test]
async fn test_conference_seat_replace_success() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let a_port = portpicker::pick_unused_port().expect("no free port");
    let a = TestUa::callee_with_username(a_port, 1, "a").await;

    let a1_port = portpicker::pick_unused_port().expect("no free port");
    let a1 = TestUa::callee_with_username(a1_port, 1, "a1").await;

    let mut ws = ws_connect(&rwi_url).await;
    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_a = Uuid::new_v4().to_string();
    let dest_a = format!("sip:a@127.0.0.1:{}", a_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_a, "destination": dest_a, "caller_id": "agent-a"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_a1 = Uuid::new_v4().to_string();
    let dest_a1 = format!("sip:a1@127.0.0.1:{}", a1_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_a1, "destination": dest_a1, "caller_id": "agent-a1"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let conf_id = format!("seat-room-{}", Uuid::new_v4());
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_id}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let (resp, events) = ws_send_recv_with_id_and_events(
        &mut ws,
        "conference.seat_replace",
        serde_json::json!({"conference_id": conf_id, "old_call_id": call_a, "new_call_id": call_a1}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    if !has_event(&events, "conference_seat_replace_started") {
        let _ = wait_for_event(&mut ws, "conference_seat_replace_started", 5).await;
    }
    if !has_event(&events, "conference_seat_replace_succeeded") {
        let _ = wait_for_event(&mut ws, "conference_seat_replace_succeeded", 5).await;
    }

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.mute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a1}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.mute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["type"], "command_failed");

    a.stop();
    a1.stop();
}

#[tokio::test]
async fn test_conference_seat_replace_failure_rolls_back() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let a_port = portpicker::pick_unused_port().expect("no free port");
    let a = TestUa::callee_with_username(a_port, 1, "a").await;

    let b_port = portpicker::pick_unused_port().expect("no free port");
    let b = TestUa::callee_with_username(b_port, 1, "b").await;

    let a1_port = portpicker::pick_unused_port().expect("no free port");
    let a1 = TestUa::callee_with_username(a1_port, 1, "a1").await;

    let mut ws = ws_connect(&rwi_url).await;
    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_a = Uuid::new_v4().to_string();
    let dest_a = format!("sip:a@127.0.0.1:{}", a_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_a, "destination": dest_a, "caller_id": "agent-a"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_a1 = Uuid::new_v4().to_string();
    let dest_a1 = format!("sip:a1@127.0.0.1:{}", a1_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_a1, "destination": dest_a1, "caller_id": "agent-a1"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_b = Uuid::new_v4().to_string();
    let dest_b = format!("sip:b@127.0.0.1:{}", b_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_b, "destination": dest_b, "caller_id": "agent-b"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let conf_id = format!("seat-room-{}", Uuid::new_v4());
    let conf_other = format!("seat-room-other-{}", Uuid::new_v4());
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_id, "max_members": 3}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_other, "max_members": 2}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_b}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_other, "call_id": call_a1}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let (resp, events) = ws_send_recv_with_id_and_events(
        &mut ws,
        "conference.seat_replace",
        serde_json::json!({"conference_id": conf_id, "old_call_id": call_a, "new_call_id": call_a1}),
    )
    .await;
    assert_eq!(resp["type"], "command_failed");
    if !has_event(&events, "conference_seat_replace_started") {
        let _ = wait_for_event(&mut ws, "conference_seat_replace_started", 5).await;
    }
    if !has_event(&events, "conference_seat_replace_failed") {
        let _ = wait_for_event(&mut ws, "conference_seat_replace_failed", 5).await;
    }

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.mute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    a.stop();
    b.stop();
    a1.stop();
}
