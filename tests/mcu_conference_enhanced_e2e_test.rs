//! MCU Enhanced Conference E2E Tests
//!
//! Tests the new conference features:
//! 1. Host role + host-can-end-all (conference.end)
//! 2. Non-host rejection when calling conference.end
//! 3. Auto-destroy when <= 1 participant remains
//! 4. Short timeout auto-destroy
//!
//! Note: Audio quality verification through the MCU mixer is covered by
//! the unit tests in conference_manager.rs and the audio_content_e2e_test.rs.
//! The RWI conference.add command registers participants in the MCU mixer
//! but the full-duplex media bridge is started by higher-level paths
//! (CC addon transfer/merge, or auto-bridge in the RWI processor).

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

async fn ws_send_recv(
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("ws_send_recv timed out for action={}", action);
        }
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
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timeout waiting for event: {}", event_type);
        }
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

async fn originate_call(
    ws: &mut WsStream,
    call_id: &str,
    dest: &str,
) {
    let resp = ws_send_recv(
        ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": dest,
            "caller_id": "test",
        }),
    )
    .await;
    assert_eq!(resp["status"], "success", "originate failed: {resp}");
    let _ = wait_for_event(ws, "call_answered", 15).await;
}

/// Test: Host can end the entire conference for all participants.
///
/// Flow:
/// 1. Create 3 SIP UAs (Alice=host, Bob, Charlie)
/// 2. Originate calls to all 3
/// 3. Create conference with host_call_id = Alice's call
/// 4. Add all 3 participants
/// 5. Host (Alice) calls conference.end -> all participants removed
/// 6. Verify conference is destroyed
#[tokio::test]
async fn test_host_end_all() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let pbx = TestPbx::start(sip_port).await;

    let alice_port = portpicker::pick_unused_port().expect("no free port");
    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let charlie_port = portpicker::pick_unused_port().expect("no free port");

    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;
    let charlie = TestUa::callee_with_username(charlie_port, 1, "charlie").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    let resp = ws_send_recv(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_alice = Uuid::new_v4().to_string();
    let call_bob = Uuid::new_v4().to_string();
    let call_charlie = Uuid::new_v4().to_string();

    originate_call(
        &mut ws,
        &call_alice,
        &format!("sip:alice@127.0.0.1:{}", alice_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_bob,
        &format!("sip:bob@127.0.0.1:{}", bob_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_charlie,
        &format!("sip:charlie@127.0.0.1:{}", charlie_port),
    )
    .await;

    // Create conference with Alice as host and 10-minute timeout
    let conf_id = format!("host-end-{}", Uuid::new_v4());
    let resp = ws_send_recv(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_alice,
            "max_duration_secs": 600,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success", "conference.create failed: {resp}");

    // Add all participants
    for call_id in [&call_alice, &call_bob, &call_charlie] {
        let resp = ws_send_recv(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id,
                "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(
            resp["status"],
            "success",
            "conference.add {} failed: {resp}",
            call_id
        );
    }

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Host (Alice) ends the conference
    let resp = ws_send_recv(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"],
        "success",
        "conference.end by host failed: {resp}"
    );

    // Verify the ConferenceEndedByHost event was broadcast
    let event = wait_for_event(&mut ws, "conference_ended_by_host", 5).await;
    assert_eq!(event["conference_ended_by_host"]["conf_id"], conf_id);
    assert_eq!(
        event["conference_ended_by_host"]["host_call_id"],
        call_alice
    );
    let removed: Vec<&str> = event["conference_ended_by_host"]["removed_call_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(removed.len(), 3, "All 3 participants should be removed");

    // Verify conference no longer exists
    let resp = ws_send_recv(
        &mut ws,
        "conference.add",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_bob,
        }),
    )
    .await;
    assert_eq!(resp["status"], "error", "conference should be destroyed after host-end");

    alice.stop();
    bob.stop();
    charlie.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pbx.stop();
    tracing::info!("test_host_end_all PASSED");
}

/// Test: Non-host cannot end the conference, but host can.
#[tokio::test]
async fn test_non_host_cannot_end_conference() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let pbx = TestPbx::start(sip_port).await;

    let alice_port = portpicker::pick_unused_port().expect("no free port");
    let bob_port = portpicker::pick_unused_port().expect("no free port");

    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let resp = ws_send_recv(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_alice = Uuid::new_v4().to_string();
    let call_bob = Uuid::new_v4().to_string();

    originate_call(
        &mut ws,
        &call_alice,
        &format!("sip:alice@127.0.0.1:{}", alice_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_bob,
        &format!("sip:bob@127.0.0.1:{}", bob_port),
    )
    .await;

    let conf_id = format!("non-host-{}", Uuid::new_v4());
    let resp = ws_send_recv(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");

    for call_id in [&call_alice, &call_bob] {
        let resp = ws_send_recv(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id,
                "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(resp["status"], "success");
    }

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Bob (non-host) tries to end the conference -> should fail
    let resp = ws_send_recv(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_bob,
        }),
    )
    .await;
    assert_eq!(
        resp["status"],
        "error",
        "Non-host should not be able to end conference: {resp}"
    );

    // Alice (host) ends the conference -> should succeed
    let resp = ws_send_recv(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"],
        "success",
        "Host should be able to end conference: {resp}"
    );

    alice.stop();
    bob.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pbx.stop();
    tracing::info!("test_non_host_cannot_end_conference PASSED");
}

/// Test: Conference auto-destroys when only 1 participant remains.
///
/// Flow:
/// 1. Create 3 UAs, originate calls, create conference, add all 3
/// 2. Remove 1 participant (3->2 remain) -> conference still alive
/// 3. Remove another participant (2->1 remain) -> auto-destroy triggered
/// 4. Verify conference is gone
#[tokio::test]
async fn test_auto_destroy_on_last_participant() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let pbx = TestPbx::start(sip_port).await;

    let alice_port = portpicker::pick_unused_port().expect("no free port");
    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let charlie_port = portpicker::pick_unused_port().expect("no free port");

    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;
    let charlie = TestUa::callee_with_username(charlie_port, 1, "charlie").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let resp = ws_send_recv(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_alice = Uuid::new_v4().to_string();
    let call_bob = Uuid::new_v4().to_string();
    let call_charlie = Uuid::new_v4().to_string();

    originate_call(
        &mut ws,
        &call_alice,
        &format!("sip:alice@127.0.0.1:{}", alice_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_bob,
        &format!("sip:bob@127.0.0.1:{}", bob_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_charlie,
        &format!("sip:charlie@127.0.0.1:{}", charlie_port),
    )
    .await;

    let conf_id = format!("auto-destroy-{}", Uuid::new_v4());
    let resp = ws_send_recv(
        &mut ws,
        "conference.create",
        serde_json::json!({"conference_id": conf_id}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    for call_id in [&call_alice, &call_bob, &call_charlie] {
        let resp = ws_send_recv(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id,
                "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(resp["status"], "success");
    }

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Remove Charlie: 3 -> 2 remain, conference still alive
    let resp = ws_send_recv(
        &mut ws,
        "conference.remove",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_charlie,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify conference still exists by muting Alice
    let resp = ws_send_recv(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"],
        "success",
        "Conference should still exist with 2 participants"
    );

    // Unmute Alice for cleanliness
    let _ = ws_send_recv(
        &mut ws,
        "conference.unmute",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;

    // Remove Bob: 2 -> 1 remain (Alice), conference stays alive
    let resp = ws_send_recv(
        &mut ws,
        "conference.remove",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_bob,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify conference is still alive with Alice
    let resp = ws_send_recv(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"], "success",
        "Conference should stay alive with 1 participant remaining"
    );

    // Remove Alice: 1 -> 0 remain, triggers auto-destroy
    let resp = ws_send_recv(
        &mut ws,
        "conference.remove",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify conference is destroyed
    let resp = ws_send_recv(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"], "error",
        "Conference should be auto-destroyed when empty"
    );

    alice.stop();
    bob.stop();
    charlie.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pbx.stop();
    tracing::info!("test_auto_destroy_on_last_participant PASSED");
}

/// Test: Conference auto-destroys after timeout.
///
/// Uses a very short timeout (3 seconds) to verify the mechanism.
#[tokio::test]
async fn test_conference_timeout_auto_destroy() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let pbx = TestPbx::start(sip_port).await;

    let alice_port = portpicker::pick_unused_port().expect("no free port");
    let bob_port = portpicker::pick_unused_port().expect("no free port");

    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let resp = ws_send_recv(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_alice = Uuid::new_v4().to_string();
    let call_bob = Uuid::new_v4().to_string();

    originate_call(
        &mut ws,
        &call_alice,
        &format!("sip:alice@127.0.0.1:{}", alice_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_bob,
        &format!("sip:bob@127.0.0.1:{}", bob_port),
    )
    .await;

    // Create conference with 3-second timeout
    let conf_id = format!("timeout-{}", Uuid::new_v4());
    let resp = ws_send_recv(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id,
            "max_duration_secs": 3,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");

    for call_id in [&call_alice, &call_bob] {
        let resp = ws_send_recv(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id,
                "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(resp["status"], "success");
    }

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify conference exists
    let resp = ws_send_recv(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success", "Conference should exist before timeout");

    // Wait for timeout + auto-destroy (3s timeout + 2s margin)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Verify conference is destroyed
    let resp = ws_send_recv(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id,
            "call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"], "error",
        "Conference should be auto-destroyed after timeout"
    );

    alice.stop();
    bob.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pbx.stop();
    tracing::info!("test_conference_timeout_auto_destroy PASSED");
}

/// Test: Conference without a host can still be destroyed normally.
/// conference.end should fail if no host was designated.
#[tokio::test]
async fn test_conference_end_without_host_fails() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let pbx = TestPbx::start(sip_port).await;

    let alice_port = portpicker::pick_unused_port().expect("no free port");
    let bob_port = portpicker::pick_unused_port().expect("no free port");

    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let resp = ws_send_recv(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(resp["status"], "success");

    let call_alice = Uuid::new_v4().to_string();
    let call_bob = Uuid::new_v4().to_string();

    originate_call(
        &mut ws,
        &call_alice,
        &format!("sip:alice@127.0.0.1:{}", alice_port),
    )
    .await;
    originate_call(
        &mut ws,
        &call_bob,
        &format!("sip:bob@127.0.0.1:{}", bob_port),
    )
    .await;

    // Create conference WITHOUT a host
    let conf_id = format!("no-host-{}", Uuid::new_v4());
    let resp = ws_send_recv(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id,
        }),
    )
    .await;
    assert_eq!(resp["status"], "success");

    for call_id in [&call_alice, &call_bob] {
        let resp = ws_send_recv(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id,
                "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(resp["status"], "success");
    }

    tokio::time::sleep(Duration::from_secs(1)).await;

    // conference.end should fail because no host was designated
    let resp = ws_send_recv(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_alice,
        }),
    )
    .await;
    assert_eq!(
        resp["status"],
        "error",
        "conference.end should fail when no host was designated: {resp}"
    );

    // conference.destroy should still work
    let resp = ws_send_recv(
        &mut ws,
        "conference.destroy",
        serde_json::json!({
            "conference_id": conf_id,
        }),
    )
    .await;
    assert_eq!(
        resp["status"],
        "success",
        "conference.destroy should work without a host: {resp}"
    );

    alice.stop();
    bob.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pbx.stop();
    tracing::info!("test_conference_end_without_host_fails PASSED");
}
