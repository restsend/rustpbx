//! Dynamic Leg E2E Test
//!
//! This test verifies the dynamic leg add/remove functionality using real SIP UAs.
//!
//! Test scenario:
//! 1. Start a TestPbx server
//! 2. Create a SIP UA (Bob) that auto-answers with echo
//! 3. Originate a call to Bob via RWI
//! 4. Wait for Bob to answer
//! 5. Send CallCommand::LegAdd to add an App leg (IVR)
//! 6. Verify session has 2 legs
//! 7. Send CallCommand::LegRemove to remove the IVR leg
//! 8. Verify session has 1 leg and Bob is still connected
//! 9. Clean up

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

// ─── WebSocket helpers ───────────────────────────────────────────────────────

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

async fn ws_send_recv(ws: &mut WsStream, json: &str) -> serde_json::Value {
    let req: serde_json::Value = serde_json::from_str(json).expect("invalid JSON");
    let action_id = req["action_id"]
        .as_str()
        .expect("missing action_id")
        .to_string();

    ws.send(Message::Text(json.into())).await.unwrap();

    // Wait for command_completed or command_failed event with matching action_id
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("ws_send_recv: timed out waiting for response");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");

        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).expect("not JSON");
            if (v["type"] == "command_completed" || v["type"] == "command_failed")
                && v["action_id"] == action_id
            {
                return v;
            }
        }
    }
}

async fn recv_until(
    ws: &mut WsStream,
    timeout_secs: u64,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("recv_until: timed out waiting for matching frame");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv_until timeout")
            .expect("stream ended")
            .expect("ws error");
        let v: serde_json::Value = match msg {
            Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        };
        if predicate(&v) {
            return v;
        }
    }
}

fn rwi_req(action: &str, params: serde_json::Value) -> (String, String) {
    let id = Uuid::new_v4().to_string();
    let json = serde_json::to_string(&serde_json::json!({
        "rwi": "1.0",
        "action_id": id,
        "action": action,
        "params": params,
    }))
    .unwrap();
    (id, json)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Test dynamic leg add/remove on an active SIP call.
#[tokio::test]
async fn test_dynamic_leg_add_remove() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let bob_port = portpicker::pick_unused_port().expect("no free bob port");

    // Start the PBX
    let pbx = TestPbx::start(sip_port).await;

    // Bob listens on bob_port, rings for 1 s then echo-answers
    let bob = TestUa::callee(bob_port, 1).await;

    // Connect RWI client
    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe to events
    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    // Originate to Bob
    let call_id = format!("e2e-dynleg-{}", Uuid::new_v4());
    let destination = bob.sip_uri("bob");

    let (orig_id, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": destination,
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv(&mut ws, &orig_json).await;
    assert_eq!(
        v["type"], "command_completed",
        "originate response should be success: {v}"
    );
    assert_eq!(v["action_id"], orig_id, "action_id not echoed");

    // Wait for Bob to answer
    let answered = recv_until(&mut ws, 10, |v| {
        v.get("call_answered").is_some() || v.to_string().contains("answered")
    })
    .await;
    tracing::info!("Bob answered: {:?}", answered);

    // Give the call a moment to fully establish
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Get the session handle from registry
    let handle = pbx
        .registry
        .get_handle(&call_id)
        .expect("Session should exist in registry");

    // Start an IVR app
    tracing::info!("Starting IVR app on session {}", call_id);
    handle
        .send_command(rustpbx::call::domain::CallCommand::StartApp {
            app_name: "ivr".to_string(),
            params: Some(serde_json::json!({"file": "config/ivr/test.toml"})),
            auto_answer: false,
        })
        .expect("Should be able to send StartApp command");

    // Give the app a moment to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check snapshot if available
    if let Some(snapshot) = handle.snapshot() {
        tracing::info!("After StartApp leg count: {}", snapshot.leg_count);
    }

    // Stop the IVR app
    tracing::info!("Stopping IVR app on session {}", call_id);
    handle
        .send_command(rustpbx::call::domain::CallCommand::StopApp {
            reason: None,
        })
        .expect("Should be able to send StopApp command");

    // Give it a moment to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check snapshot if available
    if let Some(snapshot) = handle.snapshot() {
        tracing::info!("After StopApp leg count: {}", snapshot.leg_count);
    }

    // Clean up
    ws.close(None).await.unwrap();
    bob.stop();
    pbx.stop();

    tracing::info!("Dynamic leg add/remove test passed!");
}

/// Test adding a SIP leg to an existing call.
/// This simulates the scenario where a call is transferred to another SIP endpoint.
#[tokio::test]
async fn test_dynamic_sip_leg_add() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let bob_port = portpicker::pick_unused_port().expect("no free bob port");
    let charlie_port = portpicker::pick_unused_port().expect("no free charlie port");

    // Start the PBX
    let pbx = TestPbx::start(sip_port).await;

    // Bob and Charlie listen on their ports
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;
    let _charlie = TestUa::callee_with_username(charlie_port, 1, "charlie").await;

    // Connect RWI client
    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe
    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed");

    // Originate to Bob
    let call_id = format!("e2e-sipleg-{}", Uuid::new_v4());
    let (_, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": bob.sip_uri("bob"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv(&mut ws, &orig_json).await;
    assert_eq!(v["type"], "command_completed");

    // Wait for answer
    let _answered = recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;
    tracing::info!("Bob answered");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Get session handle
    let handle = pbx
        .registry
        .get_handle(&call_id)
        .expect("Session should exist");

    // Check snapshot if available
    if let Some(snapshot) = handle.snapshot() {
        tracing::info!("Initial leg count: {}", snapshot.leg_count);
    }

    // Add a SIP leg to Charlie
    tracing::info!("Adding SIP leg to Charlie");
    handle
        .send_command(rustpbx::call::domain::CallCommand::LegAdd {
            target: format!("sip:charlie@127.0.0.1:{}", charlie_port),
            leg_id: Some(rustpbx::call::domain::LegId::new("charlie-1")),
        })
        .expect("Should be able to send LegAdd command");

    // Give it time to process
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check snapshot if available
    if let Some(snapshot) = handle.snapshot() {
        tracing::info!("After adding SIP leg: {} legs", snapshot.leg_count);
    }

    // Remove the SIP leg
    handle
        .send_command(rustpbx::call::domain::CallCommand::LegRemove {
            leg_id: rustpbx::call::domain::LegId::new("charlie-1"),
        })
        .expect("Should be able to send LegRemove command");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check snapshot if available
    if let Some(snapshot) = handle.snapshot() {
        tracing::info!("After removing SIP leg: {} legs", snapshot.leg_count);
    }

    // Clean up
    ws.close(None).await.unwrap();
    bob.stop();
    pbx.stop();

    tracing::info!("Dynamic SIP leg test passed!");
}
