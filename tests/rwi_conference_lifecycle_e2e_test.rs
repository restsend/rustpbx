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
    let action_id = Uuid::new_v4().to_string();
    let request = serde_json::json!({
        "rwi": "1.0",
        "action_id": action_id,
        "action": action,
        "params": params,
    });
    let json = serde_json::to_string(&request).unwrap();
    ws.send(Message::Text(json.into())).await.unwrap();

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

#[tokio::test]
async fn test_conference_lifecycle() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let a_port = portpicker::pick_unused_port().expect("no free port");
    let a = TestUa::callee_with_username(a_port, 1, "a").await;

    let b_port = portpicker::pick_unused_port().expect("no free port");
    let b = TestUa::callee_with_username(b_port, 1, "b").await;

    let c_port = portpicker::pick_unused_port().expect("no free port");
    let c = TestUa::callee_with_username(c_port, 1, "c").await;

    let mut ws = ws_connect(&rwi_url).await;

    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Create conference
    let conf_id = format!("conf-{}", Uuid::new_v4());
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_id, "max_members": 4}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Originate calls A, B, C
    let call_a = Uuid::new_v4().to_string();
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_a,
            "destination": format!("sip:a@127.0.0.1:{}", a_port),
            "caller_id": "host"
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_b = Uuid::new_v4().to_string();
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_b,
            "destination": format!("sip:b@127.0.0.1:{}", b_port),
            "caller_id": "host"
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_c = Uuid::new_v4().to_string();
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_c,
            "destination": format!("sip:c@127.0.0.1:{}", c_port),
            "caller_id": "host"
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Add A, B, C to conference
    for call_id in [&call_a, &call_b, &call_c] {
        let resp = ws_send_recv_with_id(
            &mut ws,
            "conference.add",
            serde_json::json!({"conference_id": conf_id, "call_id": call_id}),
        )
        .await;
        assert_eq!(
            resp["status"], "success",
            "add {} failed: {:?}",
            call_id, resp
        );
    }

    // Mute A
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.mute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Unmute A
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.unmute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Remove B
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.remove",
        serde_json::json!({"conference_id": conf_id, "call_id": call_b}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Destroy conference
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.destroy",
        serde_json::json!({"conference_id": conf_id}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    a.stop();
    b.stop();
    c.stop();
}

#[tokio::test]
async fn test_conference_rapid_add_remove_rejoin() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    let a_port = portpicker::pick_unused_port().expect("no free port");
    let a = TestUa::callee_with_username(a_port, 1, "a").await;

    let mut ws = ws_connect(&rwi_url).await;

    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Create conference
    let conf_id = format!("conf-rapid-{}", Uuid::new_v4());
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_id, "max_members": 4}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Originate call A
    let call_a = Uuid::new_v4().to_string();
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_a,
            "destination": format!("sip:a@127.0.0.1:{}", a_port),
            "caller_id": "host"
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Rapidly add, remove, and rejoin A multiple times
    for i in 0..3 {
        let resp = ws_send_recv_with_id(
            &mut ws,
            "conference.add",
            serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
        )
        .await;
        assert_eq!(
            resp["status"], "success",
            "add iteration {} failed: {:?}",
            i, resp
        );

        let resp = ws_send_recv_with_id(
            &mut ws,
            "conference.remove",
            serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
        )
        .await;
        assert_eq!(
            resp["status"], "success",
            "remove iteration {} failed: {:?}",
            i, resp
        );
    }

    // Final rejoin
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(resp["status"], "success", "final add failed: {:?}", resp);

    // Verify mute/unmute still works after rapid operations
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.mute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(
        resp["status"], "success",
        "mute after rapid ops failed: {:?}",
        resp
    );

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.unmute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_a}),
    )
    .await;
    assert_eq!(
        resp["status"], "success",
        "unmute after rapid ops failed: {:?}",
        resp
    );

    // Destroy conference
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.destroy",
        serde_json::json!({"conference_id": conf_id}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    a.stop();
}
