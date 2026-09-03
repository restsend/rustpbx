// In-process integration tests for the RWI WebSocket interface.
//
// These tests spin up a real Axum HTTP server bound to a random local port,
// then connect to it with tokio-tungstenite.  No external process is needed.
//
// Coverage:
//   • WebSocket upgrade / auth rejection (no token → 401)
//   • Full round-trip: subscribe → list_calls → response shape
//   • action_id echo — response always carries back the sent action_id
//   • Error codes: unknown_action, missing_action, not_found, not_implemented
//   • media.stop command (new in this sprint)
//   • call.unbridge command (new in this sprint)
//   • Event fan-out: second client receives event pushed by gateway

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::helpers::ws_harness::{connect, req, send_recv, send_recv_matching, start_test_server};

// ─────────────────────────────────────────────────────────────────────────────
// Auth / connection
// ─────────────────────────────────────────────────────────────────────────────

/// Connecting without a token must get 401, not a WebSocket upgrade.
#[tokio::test]
async fn test_auth_rejected_without_token() {
    let (url, _gw, _reg) = start_test_server().await;
    let result = timeout(
        Duration::from_secs(5),
        connect_async(&url), // no token
    )
    .await
    .expect("timeout");

    // tungstenite returns Err on a non-101 HTTP response
    assert!(
        result.is_err(),
        "connection without token should be rejected"
    );
}

/// A valid token must result in a successful WebSocket upgrade.
#[tokio::test]
async fn test_valid_token_connects() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;
    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// session.subscribe / session.list_calls
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_session_subscribe_returns_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["action_id"], id, "action_id must be echoed");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_session_list_calls_empty_returns_array() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["action_id"], id);
    // data should be an array (possibly empty)
    assert!(
        v["data"].is_array(),
        "list_calls data must be array, got: {}",
        v
    );

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// action_id round-trip
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_action_id_always_echoed_on_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    for _ in 0..3 {
        let (id, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["action_id"], id, "action_id must match");
    }

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_action_id_echoed_on_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req("call.answer", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert_eq!(v["action_id"], id, "action_id must be echoed even on error");

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Error codes
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_action_returns_unknown_action_code() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("totally.unknown", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("unknown_action"))
            .unwrap_or(false),
        "error should contain 'unknown_action': {}",
        v
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_missing_action_field_returns_missing_action_code() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    ws.send(Message::Text(r#"{"rwi":"1.0","params":{}}"#.into()))
        .await
        .unwrap();
    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected: {:?}", other),
    };

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("missing_action"))
            .unwrap_or(false),
        "error should contain 'missing_action': {}",
        v
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_invalid_json_returns_parse_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    ws.send(Message::Text("not json at all".into()))
        .await
        .unwrap();
    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected: {:?}", other),
    };

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("parse_error"))
            .unwrap_or(false),
        "error should contain 'parse_error': {}",
        v
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_answer_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.answer",
        serde_json::json!({"call_id": "no-such-call"}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("not found") || s.contains("Call not found"))
            .unwrap_or(false),
        "error should contain not_found: {}",
        v
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_originate_no_sip_server_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.originate",
        serde_json::json!({
            "call_id": "new-call",
            "destination": "sip:test@local",
        }),
    );
    let v = send_recv(&mut ws, &json).await;

    // Without a SIP server, originate returns command_failed
    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("Command failed") || s.contains("command"))
            .unwrap_or(false),
        "error should contain command_failed: {}",
        v
    );

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// New commands: media.stop, call.unbridge
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_media_stop_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("media.stop", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("not found") || s.contains("Call not found"))
            .unwrap_or(false),
        "error should contain not_found: {}",
        v
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_unbridge_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("call.unbridge", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("not found") || s.contains("Call not found"))
            .unwrap_or(false),
        "error should contain not_found: {}",
        v
    );

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_bridge_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.bridge",
        serde_json::json!({"leg_a": "ghost-a", "leg_b": "ghost-b"}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .map(|s| s.contains("not found") || s.contains("Call not found"))
            .unwrap_or(false),
        "error should contain not_found: {}",
        v
    );

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Multiple operations without disconnect
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sequential_commands_on_single_connection() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let commands = vec![
        (
            "session.subscribe",
            serde_json::json!({"contexts": ["ctx1", "ctx2"]}),
        ),
        ("session.list_calls", serde_json::json!({})),
        ("call.answer", serde_json::json!({"call_id": "no-call"})),
        ("call.hangup", serde_json::json!({"call_id": "no-call"})),
        ("call.ring", serde_json::json!({"call_id": "no-call"})),
        ("media.stop", serde_json::json!({"call_id": "no-call"})),
        ("call.unbridge", serde_json::json!({"call_id": "no-call"})),
    ];

    for (action, params) in commands {
        let (id, json) = req(action, params);
        let v = send_recv(&mut ws, &json).await;
        // Every response must be valid JSON with action_id echoed
        assert!(
            v["status"] == "success" || v["status"] == "error",
            "unexpected response for {}: {}",
            action,
            v
        );
        assert_eq!(v["action_id"], id, "action_id mismatch for {}", action);
    }

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Event push: gateway fan-out reaches subscribed session
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_event_pushed_from_gateway_arrives_at_client() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Subscribe to "push-ctx"
    let (_, json) = req(
        "session.subscribe",
        serde_json::json!({"contexts": ["push-ctx"]}),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    // Small delay so the gateway receives the subscription before we push
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Push a DTMF event via the gateway directly
    {
        let gw = gateway.read();
        gw.fan_out(
            "push-ctx",
            &rustpbx::rwi::Dtmf {
                call_id: "pushed-call".to_string(),
                digit: "7".to_string(),
                leg_id: None,
                extra: None,
            },
        );
    }

    // The client must receive it within 2 seconds
    let msg = timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for pushed event")
        .expect("stream ended")
        .expect("ws error");

    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected frame: {:?}", other),
    };

    let s = serde_json::to_string(&v).unwrap();
    assert!(
        s.contains("pushed-call"),
        "event should reference pushed-call: {s}"
    );
    assert!(
        s.contains("\"7\"") || s.contains("\"digit\""),
        "event should contain digit or field: {s}"
    );

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Reconnect: second connection after first closes works normally
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_reconnect_after_close() {
    let (url, _gw, _reg) = start_test_server().await;

    // First connection
    {
        let mut ws = connect(&url).await;
        let (_, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["status"], "success");
        ws.close(None).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second connection on the same server must also work
    {
        let mut ws = connect(&url).await;
        let (_, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["status"], "success");
        ws.close(None).await.unwrap();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Conference command tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_conference_create_returns_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal",
            "max_members": 10
        }),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["conf_id"], "room-1");
}

#[tokio::test]
async fn test_conference_create_duplicate_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create first conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to create duplicate
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().unwrap_or("").contains("already exists"));
}

#[tokio::test]
async fn test_conference_destroy_returns_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Destroy conference
    let (action_id, json) = req(
        "conference.destroy",
        serde_json::json!({
            "conf_id": "room-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["conf_id"], "room-1");
}

#[tokio::test]
async fn test_conference_add_not_found_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Try to add call to non-existent conference
    let (_, json) = req(
        "conference.add",
        serde_json::json!({
            "conf_id": "nonexistent",
            "call_id": "call-1"
        }),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().unwrap_or("").contains("not found"));
}

#[tokio::test]
async fn test_conference_mute_not_in_conference_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to mute call that's not in conference
    let (action_id, json) = req(
        "conference.mute",
        serde_json::json!({
            "conf_id": "room-1",
            "call_id": "call-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .unwrap_or("")
            .contains("is not in conference")
    );
}

#[tokio::test]
async fn test_conference_unmute_not_in_conference_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to unmute call that's not in conference
    let (action_id, json) = req(
        "conference.unmute",
        serde_json::json!({
            "conf_id": "room-1",
            "call_id": "call-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .unwrap_or("")
            .contains("is not in conference")
    );
}

#[tokio::test]
async fn test_conference_remove_not_in_conference_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to remove call that's not in conference
    let (action_id, json) = req(
        "conference.remove",
        serde_json::json!({
            "conf_id": "room-1",
            "call_id": "call-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"]
            .as_str()
            .unwrap_or("")
            .contains("is not in conference")
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Session Resume & Event Replay
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_session_resume_returns_events() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Cache some events in the gateway
    {
        let gw = gateway.read();
        let event1 = rustpbx::rwi::event::to_legacy_event(
            &rustpbx::rwi::CallRinging {
                call_id: "test-call-1".to_string(),
                early_media: false,
            },
            None,
        );
        let event2 = rustpbx::rwi::event::to_legacy_event(
            &rustpbx::rwi::CallAnswered {
                call_id: "test-call-1".to_string(),
            },
            None,
        );
        gw.cache_event(&"test-call-1".to_string(), &event1);
        gw.cache_event(&"test-call-1".to_string(), &event2);
    }

    // Request session resume (should return all cached events)
    let (_, json) = req("session.resume", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    // Debug: print response if error
    if v["status"] == "error" {
        eprintln!("Session resume error: {:?}", v);
    }

    assert_eq!(v["status"], "success");
    assert!(v["data"]["events"].is_array(), "events should be an array");
    assert!(
        v["data"]["replayed_count"].is_u64(),
        "replayed_count should be a number"
    );

    // Should have cached events
    let replayed = v["data"]["replayed_count"].as_u64().unwrap();
    assert!(replayed >= 2, "should have at least 2 cached events");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_resume_returns_call_specific_events() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Cache events for different calls
    {
        let gw = gateway.read();
        let event1 = rustpbx::rwi::event::to_legacy_event(
            &rustpbx::rwi::CallRinging {
                call_id: "call-a".to_string(),
                early_media: false,
            },
            None,
        );
        let event2 = rustpbx::rwi::event::to_legacy_event(
            &rustpbx::rwi::CallRinging {
                call_id: "call-b".to_string(),
                early_media: false,
            },
            None,
        );
        gw.cache_event(&"call-a".to_string(), &event1);
        gw.cache_event(&"call-b".to_string(), &event2);
    }

    // Request call resume for specific call
    let (_, json) = req("call.resume", serde_json::json!({"call_id": "call-a"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["call_id"], "call-a");
    assert!(v["data"]["events"].is_array());

    // Should only have events for call-a
    let events = v["data"]["events"].as_array().unwrap();
    for event in events {
        assert_eq!(
            event["call_id"], "call-a",
            "should only have events for call-a"
        );
    }

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Leg Timeline CDR Enhancement Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_leg_timeline_serialization() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};

    let mut timeline = LegTimeline::new();

    // Add some events
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({"source": "originate"})),
    );

    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Bridged,
        Some("leg-2".to_string()),
        None,
    );

    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Removed,
        None,
        Some(serde_json::json!({"reason": "hangup"})),
    );

    // Serialize to JSON
    let json = serde_json::to_value(&timeline).unwrap();

    assert!(json["events"].is_array());
    let events = json["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);

    // Verify event structure (using camelCase due to #[serde(rename_all = "camelCase")])
    assert_eq!(events[0]["legId"], "leg-1");
    assert_eq!(events[0]["eventType"], "added");
    assert_eq!(events[1]["eventType"], "bridged");
    assert_eq!(events[1]["peerLegId"], "leg-2");
}

#[tokio::test]
async fn test_leg_timeline_is_empty() {
    use rustpbx::callrecord::LegTimeline;

    let timeline = LegTimeline::new();
    assert!(timeline.is_empty());

    let mut timeline_with_events = LegTimeline::new();
    timeline_with_events.add_event(
        "leg-1".to_string(),
        rustpbx::callrecord::LegTimelineEventType::Added,
        None,
        None,
    );
    assert!(!timeline_with_events.is_empty());
}
