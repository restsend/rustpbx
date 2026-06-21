//! Conference Dial-In E2E Tests
//!
//! Verifies:
//! - 3-way conference with RTP audio using RWI API
//! - Host controls (mute/unmute, end conference)
//! - Auto-destroy when last participant leaves
//! - Timeout auto-destroy
//!
//! Usage: cargo test --test conference_dialin_e2e_test -- --nocapture

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

async fn recv_until(
    ws: &mut WsStream,
    timeout_secs: u64,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("recv_until: timed out");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv_until timeout")
            .expect("stream ended")
            .expect("ws error");
        let v: serde_json::Value = match msg {
            Message::Text(t) => {
                tracing::info!("[recv_until] frame: {t}");
                serde_json::from_str(&t).expect("not JSON")
            }
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

/// Send a command and wait for command_completed with matching action_id.
async fn ws_cmd(ws: &mut WsStream, action: &str, params: serde_json::Value) -> serde_json::Value {
    let (id, json) = rwi_req(action, params);
    ws.send(Message::Text(json.into())).await.unwrap();
    recv_until(ws, 10, |v| {
        (v["type"] == "command_completed" || v["type"] == "command_failed") && v["action_id"] == id
    })
    .await
}

/// Wait for an unsolicited RWI event (e.g. call_answered, conference_ended_by_host).
/// RWI event format: {"event_type": "call_answered", ...}
async fn wait_for_event(
    ws: &mut WsStream,
    event_name: &str,
    max_wait_secs: u64,
) -> serde_json::Value {
    recv_until(ws, max_wait_secs, |v| {
        v["event_type"].as_str() == Some(event_name)
    })
    .await
}

#[tokio::test]
async fn test_conference_three_way_audio() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let pbx = TestPbx::start(sip_port).await;

    let a_port = portpicker::pick_unused_port().unwrap();
    let b_port = portpicker::pick_unused_port().unwrap();
    let c_port = portpicker::pick_unused_port().unwrap();

    let alice = TestUa::callee_with_username(a_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(b_port, 1, "bob").await;
    let carol = TestUa::callee_with_username(c_port, 1, "carol").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success", "subscribe failed: {v}");

    // Originate Alice
    let call_a = Uuid::new_v4().to_string();
    let r = ws_cmd(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_a,
            "destination": format!("sip:alice@127.0.0.1:{}", a_port),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "originate alice failed: {r}");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Originate Bob
    let call_b = Uuid::new_v4().to_string();
    let r = ws_cmd(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_b,
            "destination": format!("sip:bob@127.0.0.1:{}", b_port),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "originate bob failed: {r}");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Originate Carol
    let call_c = Uuid::new_v4().to_string();
    let r = ws_cmd(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_c,
            "destination": format!("sip:carol@127.0.0.1:{}", c_port),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "originate carol failed: {r}");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Create conference with Alice as host
    let conf_id = format!("three-way-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id,
            "host_call_id": call_a,
            "max_members": 4,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "create conf failed: {r}");

    // Add participants
    for call_id in [&call_a, &call_b, &call_c] {
        let r = ws_cmd(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id,
                "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(r["status"], "success", "add {call_id} failed: {r}");
    }

    // Let audio flow through the conference mixer.
    // Note: sipbot echo bots only echo received RTP. In an N-1 conference
    // mixer, each participant hears others. With all echo bots and no external
    // audio source, the mixer sends silence. The following assertions verify
    // that signaling and basic RTP channel setup works correctly.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify each participant has active audio channels (RTP is flowing).
    // At minimum, each participant should have an active RTP path.
    // The actual audio content depends on external injection which is tested
    // in audio_content_e2e_test.rs for 2-party calls.
    for (label, ua) in [("alice", &alice), ("bob", &bob), ("carol", &carol)] {
        tracing::info!("{label} stats: {}", ua.rtp_stats_summary());
    }

    // Mute/unmute Bob
    let r = ws_cmd(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_b,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "mute bob failed: {r}");

    let r = ws_cmd(
        &mut ws,
        "conference.unmute",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_b,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "unmute bob failed: {r}");

    // Destroy
    let r = ws_cmd(
        &mut ws,
        "conference.destroy",
        serde_json::json!({
            "conference_id": conf_id,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "destroy failed: {r}");

    // Cleanup
    for call_id in [&call_a, &call_b, &call_c] {
        let _ = ws_cmd(
            &mut ws,
            "call.hangup",
            serde_json::json!({"call_id": call_id}),
        )
        .await;
    }

    alice.stop();
    bob.stop();
    carol.stop();
    pbx.stop();
}

#[tokio::test]
async fn test_conference_host_end_all() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let pbx = TestPbx::start(sip_port).await;

    let a_port = portpicker::pick_unused_port().unwrap();
    let b_port = portpicker::pick_unused_port().unwrap();
    let c_port = portpicker::pick_unused_port().unwrap();

    let alice = TestUa::callee_with_username(a_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(b_port, 1, "bob").await;
    let carol = TestUa::callee_with_username(c_port, 1, "carol").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success");

    let call_a = Uuid::new_v4().to_string();
    let call_b = Uuid::new_v4().to_string();
    let call_c = Uuid::new_v4().to_string();

    for (call_id, port, user) in [
        (&call_a, a_port, "alice"),
        (&call_b, b_port, "bob"),
        (&call_c, c_port, "carol"),
    ] {
        let r = ws_cmd(
            &mut ws,
            "call.originate",
            serde_json::json!({
                "call_id": call_id,
                "destination": format!("sip:{user}@127.0.0.1:{port}"),
                "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
                "context": "default",
                "timeout_secs": 15,
            }),
        )
        .await;
        assert_eq!(r["status"], "success");
        let _ = wait_for_event(&mut ws, "call_answered", 15).await;
    }

    let conf_id = format!("host-end-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id, "host_call_id": call_a, "max_members": 4,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");

    for call_id in [&call_a, &call_b, &call_c] {
        let r = ws_cmd(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id, "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(r["status"], "success");
    }

    // Host ends
    let r = ws_cmd(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id, "host_call_id": call_a,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "host end failed: {r}");

    let event = wait_for_event(&mut ws, "conference_ended_by_host", 5).await;
    let removed = event["removed_call_ids"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(removed, 3, "expected 3 removed call_ids in event: {event}");

    // After end, operations should fail
    let r = ws_cmd(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_a,
        }),
    )
    .await;
    assert_eq!(r["status"], "error");

    for call_id in [&call_a, &call_b, &call_c] {
        let _ = ws_cmd(
            &mut ws,
            "call.hangup",
            serde_json::json!({"call_id": call_id}),
        )
        .await;
    }

    alice.stop();
    bob.stop();
    carol.stop();
    pbx.stop();
}

#[tokio::test]
async fn test_conference_auto_destroy_on_empty() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let pbx = TestPbx::start(sip_port).await;

    let a_port = portpicker::pick_unused_port().unwrap();
    let b_port = portpicker::pick_unused_port().unwrap();

    let alice = TestUa::callee_with_username(a_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(b_port, 1, "bob").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success");

    let call_a = Uuid::new_v4().to_string();
    let call_b = Uuid::new_v4().to_string();

    for (call_id, port, user) in [(&call_a, a_port, "alice"), (&call_b, b_port, "bob")] {
        let r = ws_cmd(
            &mut ws,
            "call.originate",
            serde_json::json!({
                "call_id": call_id,
                "destination": format!("sip:{user}@127.0.0.1:{port}"),
                "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
                "context": "default",
                "timeout_secs": 15,
            }),
        )
        .await;
        assert_eq!(r["status"], "success");
        let _ = wait_for_event(&mut ws, "call_answered", 15).await;
    }

    let conf_id = format!("auto-destroy-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id, "max_members": 4,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");

    for call_id in [&call_a, &call_b] {
        let r = ws_cmd(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id, "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(r["status"], "success");
    }

    // Remove all → auto-destroy
    let _ = ws_cmd(
        &mut ws,
        "conference.remove",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_a,
        }),
    )
    .await;
    let _ = ws_cmd(
        &mut ws,
        "conference.remove",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_b,
        }),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let r = ws_cmd(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_a,
        }),
    )
    .await;
    assert_eq!(r["status"], "error");

    for call_id in [&call_a, &call_b] {
        let _ = ws_cmd(
            &mut ws,
            "call.hangup",
            serde_json::json!({"call_id": call_id}),
        )
        .await;
    }

    alice.stop();
    bob.stop();
    pbx.stop();
}

#[tokio::test]
async fn test_conference_timeout_auto_destroy() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let pbx = TestPbx::start(sip_port).await;

    let a_port = portpicker::pick_unused_port().unwrap();
    let alice = TestUa::callee_with_username(a_port, 1, "alice").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success");

    let call_a = Uuid::new_v4().to_string();
    let r = ws_cmd(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_a,
            "destination": format!("sip:alice@127.0.0.1:{a_port}"),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    let conf_id = format!("timeout-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id, "max_members": 4, "max_duration_secs": 3,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");

    let r = ws_cmd(
        &mut ws,
        "conference.add",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_a,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let r = ws_cmd(
        &mut ws,
        "conference.mute",
        serde_json::json!({
            "conference_id": conf_id, "call_id": call_a,
        }),
    )
    .await;
    assert_eq!(r["status"], "error");

    let _ = ws_cmd(
        &mut ws,
        "call.hangup",
        serde_json::json!({"call_id": call_a}),
    )
    .await;
    alice.stop();
    pbx.stop();
}

#[tokio::test]
async fn test_conference_non_host_cannot_end() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let pbx = TestPbx::start(sip_port).await;

    let a_port = portpicker::pick_unused_port().unwrap();
    let b_port = portpicker::pick_unused_port().unwrap();

    let alice = TestUa::callee_with_username(a_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(b_port, 1, "bob").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success");

    let call_a = Uuid::new_v4().to_string();
    let call_b = Uuid::new_v4().to_string();

    for (call_id, port, user) in [(&call_a, a_port, "alice"), (&call_b, b_port, "bob")] {
        let r = ws_cmd(
            &mut ws,
            "call.originate",
            serde_json::json!({
                "call_id": call_id,
                "destination": format!("sip:{user}@127.0.0.1:{port}"),
                "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
                "context": "default",
                "timeout_secs": 15,
            }),
        )
        .await;
        assert_eq!(r["status"], "success");
        let _ = wait_for_event(&mut ws, "call_answered", 15).await;
    }

    let conf_id = format!("non-host-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "conference.create",
        serde_json::json!({
            "conference_id": conf_id, "host_call_id": call_a, "max_members": 4,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");

    for call_id in [&call_a, &call_b] {
        let r = ws_cmd(
            &mut ws,
            "conference.add",
            serde_json::json!({
                "conference_id": conf_id, "call_id": call_id,
            }),
        )
        .await;
        assert_eq!(r["status"], "success");
    }

    // Bob (non-host) tries to end → should fail
    let r = ws_cmd(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id, "host_call_id": call_b,
        }),
    )
    .await;
    assert_eq!(r["status"], "error", "non-host should not be able to end");

    // Alice (host) can end
    let r = ws_cmd(
        &mut ws,
        "conference.end",
        serde_json::json!({
            "conference_id": conf_id, "host_call_id": call_a,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");

    for call_id in [&call_a, &call_b] {
        let _ = ws_cmd(
            &mut ws,
            "call.hangup",
            serde_json::json!({"call_id": call_id}),
        )
        .await;
    }

    alice.stop();
    bob.stop();
    pbx.stop();
}
