// E2E tests for Event Replay & Recovery
//
// These tests verify:
// 1. Session resume returns cached events after disconnect
// 2. Call resume returns call-specific events

use std::time::Duration;

use crate::helpers::ws_harness::{connect, req, send_recv, send_recv_matching, start_test_server};

/// Test: Full session resume flow after disconnect
/// 1. Connect client and subscribe to context
/// 2. Push events to gateway
/// 3. Disconnect
/// 4. Reconnect and resume session
/// 5. Verify events are replayed
#[tokio::test]
async fn test_full_session_resume_flow() {
    let (url, gateway, _reg) = start_test_server().await;

    // First connection - subscribe and generate events
    let session_events = {
        let mut ws = connect(&url).await;

        // Subscribe to context
        let (_, json) = req(
            "session.subscribe",
            serde_json::json!({"contexts": ["resume-test"]}),
        );
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["status"], "success");

        // Push some events via gateway
        {
            let gw = gateway.read();
            gw.fan_out(
                "resume-test",
                &rustpbx::rwi::CallCreated {
                    call_id: "resume-call-1".to_string(),
                    context: "resume-test".to_string(),
                    caller: "sip:alice@test.com".to_string(),
                    callee: "sip:bob@test.com".to_string(),
                    trunk: None,
                    sip_headers: std::collections::HashMap::new(),
                    caller_name: None,
                    callee_name: None,
                    called_phone: None,
                    app_id: None,
                    routing_target: None,
                    uuid: None,
                    routing_path: None,
                },
            );
            gw.fan_out(
                "resume-test",
                &rustpbx::rwi::CallRinging {
                    call_id: "resume-call-1".to_string(),
                    early_media: false,
                },
            );
            gw.fan_out(
                "resume-test",
                &rustpbx::rwi::CallAnswered {
                    call_id: "resume-call-1".to_string(),
                },
            );
        }

        // Give time for events to be cached
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Snapshot replayed count before disconnect
        let (action_id, json) = req("session.resume", serde_json::json!({}));
        let v = send_recv_matching(&mut ws, &json, &action_id).await;
        assert_eq!(v["status"], "success", "session.resume should succeed");
        let initial_count = v["data"]["replayed_count"].as_u64().unwrap();

        ws.close(None).await.unwrap();
        initial_count
    };

    // Wait for disconnect to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second connection - resume session
    {
        let mut ws = connect(&url).await;

        // Resume (should get all cached events)
        let (_, json) = req("session.resume", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;

        assert_eq!(v["status"], "success");
        let replayed_count = v["data"]["replayed_count"].as_u64().unwrap();

        // Should have at least the events we pushed
        assert!(
            replayed_count >= session_events,
            "Should replay at least {} events, got {}",
            session_events,
            replayed_count
        );

        // Verify event structure
        let events = v["data"]["events"].as_array().unwrap();
        assert!(!events.is_empty(), "Should have events");

        // Verify each event has required fields
        for event in events {
            assert!(
                event["timestamp"].is_string(),
                "Event should have timestamp"
            );
            assert!(event["call_id"].is_string(), "Event should have call_id");
            assert!(event["event"].is_object(), "Event should have event data");
        }

        ws.close(None).await.unwrap();
    }
}

/// Test: Call-specific resume
/// 1. Push events for multiple calls
/// 2. Resume specific call
/// 3. Verify only that call's events are returned
#[tokio::test]
async fn test_call_resume_filters_by_call_id() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Push events for multiple calls
    {
        let gw = gateway.read();

        // Call A events
        gw.cache_event(
            &"call-a".to_string(),
            &rustpbx::rwi::event::to_legacy_event(
                &rustpbx::rwi::CallRinging {
                    call_id: "call-a".to_string(),
                    early_media: false,
                },
                None,
            ),
        );
        gw.cache_event(
            &"call-a".to_string(),
            &rustpbx::rwi::event::to_legacy_event(
                &rustpbx::rwi::CallAnswered {
                    call_id: "call-a".to_string(),
                },
                None,
            ),
        );

        // Call B events
        gw.cache_event(
            &"call-b".to_string(),
            &rustpbx::rwi::event::to_legacy_event(
                &rustpbx::rwi::CallRinging {
                    call_id: "call-b".to_string(),
                    early_media: false,
                },
                None,
            ),
        );
        gw.cache_event(
            &"call-b".to_string(),
            &rustpbx::rwi::event::to_legacy_event(
                &rustpbx::rwi::CallBridged {
                    leg_a: "call-b".to_string(),
                    leg_b: "call-c".to_string(),
                },
                None,
            ),
        );
    }

    // Resume call-a specifically
    let (_, json) = req("call.resume", serde_json::json!({"call_id": "call-a"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["call_id"], "call-a");

    let events = v["data"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "Should have exactly 2 events for call-a");

    // Verify all events are for call-a
    for event in events {
        assert_eq!(event["call_id"], "call-a");
    }

    // Resume call-b
    let (_, json) = req("call.resume", serde_json::json!({"call_id": "call-b"}));
    let v = send_recv(&mut ws, &json).await;

    let events = v["data"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "Should have exactly 2 events for call-b");

    // Verify one event is bridged (checking event object contains bridged data)
    let has_bridged = events.iter().any(|e| {
        let event_json = serde_json::to_string(&e["event"]).unwrap_or_default();
        event_json.contains("bridged") || e["event"]["leg_a"].is_string()
    });

    assert!(has_bridged, "Should have a bridged event: {:?}", events);

    ws.close(None).await.unwrap();
}

/// Test: Resume with non-existent call returns empty events
#[tokio::test]
async fn test_call_resume_nonexistent_call() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.resume",
        serde_json::json!({"call_id": "non-existent-call"}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["call_id"], "non-existent-call");

    let events = v["data"]["events"].as_array().unwrap();
    assert!(
        events.is_empty(),
        "Should have no events for non-existent call"
    );
    assert_eq!(v["data"]["replayed_count"], 0);

    ws.close(None).await.unwrap();
}
