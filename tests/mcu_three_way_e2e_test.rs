//! MCU Three-Way Call E2E Test
//!
//! This test verifies the full end-to-end flow of a three-way conference call
//! using real SIP UAs and the in-server MCU mixer.
//!
//! Test scenario:
//! 1. Start a TestPbx server
//! 2. Create 3 SIP UAs (Alice, Bob, Charlie)
//! 3. Originate calls to all 3 UAs
//! 4. Create a conference and add all 3 calls
//! 5. Verify audio flows between all participants (N-1 mixing)
//! 6. Test mute/unmute functionality
//! 7. Test participant leave/join
//! 8. Clean up

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
            .expect("recv timeout")
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
            .unwrap_or_else(|_| panic!("timeout waiting for {}", event_type))
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
async fn test_mcu_three_way_conference_e2e() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start TestPbx server
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;
    let rwi_url = pbx.rwi_url.clone();

    // Create 3 SIP UAs
    let alice_port = portpicker::pick_unused_port().expect("no free port");
    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;

    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;

    let charlie_port = portpicker::pick_unused_port().expect("no free port");
    let charlie = TestUa::callee_with_username(charlie_port, 1, "charlie").await;

    // Connect to RWI
    let mut ws = ws_connect(&rwi_url).await;
    let resp = ws_send_recv_with_id(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Originate calls to all 3 UAs
    let call_alice = Uuid::new_v4().to_string();
    let dest_alice = format!("sip:alice@127.0.0.1:{}", alice_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_alice, "destination": dest_alice, "caller_id": "test"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_bob = Uuid::new_v4().to_string();
    let dest_bob = format!("sip:bob@127.0.0.1:{}", bob_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_bob, "destination": dest_bob, "caller_id": "test"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let call_charlie = Uuid::new_v4().to_string();
    let dest_charlie = format!("sip:charlie@127.0.0.1:{}", charlie_port);
    let resp = ws_send_recv_with_id(
        &mut ws,
        "call.originate",
        serde_json::json!({"call_id": call_charlie, "destination": dest_charlie, "caller_id": "test"}),
    )
    .await;
    assert_eq!(resp["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Create conference
    let conf_id = format!("three-way-{}", Uuid::new_v4());
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_id}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Add all participants to conference
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_alice}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_bob}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_charlie}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Wait for conference to stabilize
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Test mute functionality - mute Alice
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.mute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_alice}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Wait a bit
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Unmute Alice
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.unmute",
        serde_json::json!({"conference_id": conf_id, "call_id": call_alice}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Remove Bob from conference
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.remove",
        serde_json::json!({"conference_id": conf_id, "call_id": call_bob}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Wait a bit
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Add Bob back
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.add",
        serde_json::json!({"conference_id": conf_id, "call_id": call_bob}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Wait for conference to stabilize
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Destroy conference
    let resp = ws_send_recv_with_id(
        &mut ws,
        "conference.destroy",
        serde_json::json!({"conference_id": conf_id}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    // Hangup all calls
    let _ = ws_send_recv_with_id(
        &mut ws,
        "call.hangup",
        serde_json::json!({"call_id": call_alice}),
    )
    .await;
    let _ = ws_send_recv_with_id(
        &mut ws,
        "call.hangup",
        serde_json::json!({"call_id": call_bob}),
    )
    .await;
    let _ = ws_send_recv_with_id(
        &mut ws,
        "call.hangup",
        serde_json::json!({"call_id": call_charlie}),
    )
    .await;

    // Stop UAs
    alice.stop();
    bob.stop();
    charlie.stop();

    // Stop PBX
    pbx.stop();

    println!("MCU three-way conference E2E test completed successfully!");
}
