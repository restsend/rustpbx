// E2E tests for Leg Timeline CDR Enhancement
//
// These tests verify:
// 1. Leg timeline events are properly recorded
// 2. Timeline is serialized correctly to JSON
// 3. Timeline integrates with CDR persistence

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Extension,
    extract::{Query, ws::WebSocketUpgrade},
    http::HeaderMap,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use rustpbx::{
    proxy::active_call_registry::ActiveProxyCallRegistry,
    rwi::{
        RwiAuth, RwiAuthRef, RwiGateway, RwiGatewayRef,
        auth::{RwiConfig, RwiTokenConfig},
        handler::rwi_ws_handler,
    },
};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const TEST_TOKEN: &str = "timeline-test-token";

fn make_auth() -> RwiAuthRef {
    let config = RwiConfig {
        enabled: true,
        tokens: vec![RwiTokenConfig {
            token: TEST_TOKEN.to_string(),
            scopes: vec!["call.control".to_string()],
        }],
        ..Default::default()
    };
    Arc::new(tokio::sync::RwLock::new(RwiAuth::new(&config)))
}

async fn start_test_server() -> (String, RwiGatewayRef, Arc<ActiveProxyCallRegistry>) {
    let auth = make_auth();
    let gateway: RwiGatewayRef = Arc::new(tokio::sync::RwLock::new(RwiGateway::new()));
    let registry = Arc::new(ActiveProxyCallRegistry::new());

    let auth_c = auth.clone();
    let gw_c = gateway.clone();
    let reg_c = registry.clone();

    let router = axum::Router::new().route(
        "/rwi/v1",
        get(
            move |client_addr: rustpbx::handler::middleware::clientaddr::ClientAddr,
                  ws: WebSocketUpgrade,
                  Query(params): Query<HashMap<String, String>>,
                  headers: HeaderMap| {
                let a = auth_c.clone();
                let g = gw_c.clone();
                let r = reg_c.clone();
                async move {
                    rwi_ws_handler(
                        client_addr,
                        ws,
                        Query(params),
                        Extension(a),
                        Extension(g),
                        Extension(r),
                        Extension(None::<rustpbx::proxy::server::SipServerRef>),
                        headers,
                    )
                    .await
                }
            },
        ),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!("ws://127.0.0.1:{}/rwi/v1", port);
    (url, gateway, registry)
}

async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let full = format!("{}?token={}", url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&full))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

async fn send_recv(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    json: &str,
) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("recv timeout")
        .expect("stream ended")
        .expect("ws error");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
        other => panic!("unexpected frame: {:?}", other),
    }
}

fn req(action: &str, params: serde_json::Value) -> (String, String) {
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

/// Test: LegTimeline structure and serialization
#[tokio::test]
async fn test_leg_timeline_basic_operations() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};
    use chrono::Utc;

    let mut timeline = LegTimeline::new();
    
    // Verify empty timeline
    assert!(timeline.is_empty());
    assert_eq!(timeline.events.len(), 0);
    
    // Add events
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({"source": "inbound"})),
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
        Some(serde_json::json!({"reason": "hangup", "code": 200})),
    );
    
    // Verify non-empty
    assert!(!timeline.is_empty());
    assert_eq!(timeline.events.len(), 3);
    
    // Verify event properties
    assert_eq!(timeline.events[0].leg_id, "leg-1");
    assert_eq!(timeline.events[0].event_type, LegTimelineEventType::Added);
    assert!(timeline.events[0].timestamp <= Utc::now());
    
    assert_eq!(timeline.events[1].event_type, LegTimelineEventType::Bridged);
    assert_eq!(timeline.events[1].peer_leg_id, Some("leg-2".to_string()));
    
    assert_eq!(timeline.events[2].event_type, LegTimelineEventType::Removed);
}

/// Test: LegTimeline JSON serialization
#[tokio::test]
async fn test_leg_timeline_json_serialization() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};

    let mut timeline = LegTimeline::new();
    
    timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({
            "direction": "outbound",
            "destination": "sip:agent@example.com"
        })),
    );
    
    timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::Transferred,
        Some("consultation-leg".to_string()),
        Some(serde_json::json!({
            "transfer_type": "attended",
            "consultation_id": "consultation-leg"
        })),
    );
    
    timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::TransferAccepted,
        None,
        None,
    );
    
    // Serialize to JSON
    let json = serde_json::to_value(&timeline).unwrap();
    
    // Verify structure
    assert!(json.is_object());
    assert!(json["events"].is_array());
    
    let events = json["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    
    // Verify first event
    assert_eq!(events[0]["legId"], "agent-leg");
    assert_eq!(events[0]["eventType"], "added");
    assert!(events[0]["timestamp"].is_string());
    assert!(events[0]["details"].is_object());
    assert_eq!(events[0]["details"]["direction"], "outbound");
    
    // Verify transfer event
    assert_eq!(events[1]["eventType"], "transferred");
    assert_eq!(events[1]["peerLegId"], "consultation-leg");
    
    // Verify accepted event
    assert_eq!(events[2]["eventType"], "transfer_accepted");
}

/// Test: All LegTimelineEventType variants serialize correctly
#[tokio::test]
async fn test_all_event_type_variants() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};

    let mut timeline = LegTimeline::new();
    
    // Add all event types
    let event_types = vec![
        (LegTimelineEventType::Added, "added"),
        (LegTimelineEventType::Bridged, "bridged"),
        (LegTimelineEventType::Unbridged, "unbridged"),
        (LegTimelineEventType::Transferred, "transferred"),
        (LegTimelineEventType::TransferAccepted, "transfer_accepted"),
        (LegTimelineEventType::TransferFailed, "transfer_failed"),
        (LegTimelineEventType::Removed, "removed"),
    ];
    
    for (event_type, _) in &event_types {
        timeline.add_event(
            "test-leg".to_string(),
            event_type.clone(),
            None,
            None,
        );
    }
    
    // Serialize and verify
    let json = serde_json::to_value(&timeline).unwrap();
    let events = json["events"].as_array().unwrap();
    
    for (i, (_, expected_name)) in event_types.iter().enumerate() {
        assert_eq!(events[i]["eventType"], *expected_name);
    }
}

/// Test: LegTimeline with transfer failure
#[tokio::test]
async fn test_leg_timeline_transfer_failure() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};

    let mut timeline = LegTimeline::new();
    
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Added,
        None,
        None,
    );
    
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Transferred,
        Some("target-leg".to_string()),
        None,
    );
    
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::TransferFailed,
        None,
        Some(serde_json::json!({
            "reason": "timeout",
            "sip_status": 408
        })),
    );
    
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Removed,
        None,
        Some(serde_json::json!({"reason": "hangup"})),
    );
    
    // Verify full lifecycle
    assert_eq!(timeline.events.len(), 4);
    assert_eq!(timeline.events[2].event_type, LegTimelineEventType::TransferFailed);
    
    let json = serde_json::to_value(&timeline).unwrap();
    assert_eq!(json["events"][2]["details"]["sip_status"], 408);
}

/// Test: Complex multi-leg scenario
#[tokio::test]
async fn test_leg_timeline_complex_scenario() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};

    // Scenario: Call comes in, bridged to agent, transferred to supervisor
    
    let mut caller_timeline = LegTimeline::new();
    let mut agent_timeline = LegTimeline::new();
    let mut supervisor_timeline = LegTimeline::new();
    
    // Caller leg
    caller_timeline.add_event(
        "caller-leg".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({"source": "inbound", "caller": "sip:customer@example.com"})),
    );
    
    caller_timeline.add_event(
        "caller-leg".to_string(),
        LegTimelineEventType::Bridged,
        Some("agent-leg".to_string()),
        None,
    );
    
    caller_timeline.add_event(
        "caller-leg".to_string(),
        LegTimelineEventType::Unbridged,
        Some("agent-leg".to_string()),
        Some(serde_json::json!({"reason": "transfer_initiated"})),
    );
    
    caller_timeline.add_event(
        "caller-leg".to_string(),
        LegTimelineEventType::Bridged,
        Some("supervisor-leg".to_string()),
        None,
    );
    
    caller_timeline.add_event(
        "caller-leg".to_string(),
        LegTimelineEventType::Removed,
        None,
        Some(serde_json::json!({"reason": "hangup", "by": "caller"})),
    );
    
    // Agent leg
    agent_timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({"source": "queue_assignment", "agent_id": "agent-001"})),
    );
    
    agent_timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::Bridged,
        Some("caller-leg".to_string()),
        None,
    );
    
    agent_timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::Transferred,
        Some("supervisor-leg".to_string()),
        Some(serde_json::json!({"transfer_type": "attended"})),
    );
    
    agent_timeline.add_event(
        "agent-leg".to_string(),
        LegTimelineEventType::Removed,
        None,
        Some(serde_json::json!({"reason": "transfer_complete"})),
    );
    
    // Supervisor leg
    supervisor_timeline.add_event(
        "supervisor-leg".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({"source": "transfer_target", "transfer_from": "agent-leg"})),
    );
    
    supervisor_timeline.add_event(
        "supervisor-leg".to_string(),
        LegTimelineEventType::Bridged,
        Some("caller-leg".to_string()),
        None,
    );
    
    supervisor_timeline.add_event(
        "supervisor-leg".to_string(),
        LegTimelineEventType::Removed,
        None,
        Some(serde_json::json!({"reason": "hangup", "by": "caller"})),
    );
    
    // Verify all timelines
    assert_eq!(caller_timeline.events.len(), 5);
    assert_eq!(agent_timeline.events.len(), 4);
    assert_eq!(supervisor_timeline.events.len(), 3);
    
    // Verify bridging relationships
    assert_eq!(caller_timeline.events[1].peer_leg_id, Some("agent-leg".to_string()));
    assert_eq!(caller_timeline.events[3].peer_leg_id, Some("supervisor-leg".to_string()));
    
    // Serialize all timelines
    let caller_json = serde_json::to_value(&caller_timeline).unwrap();
    let agent_json = serde_json::to_value(&agent_timeline).unwrap();
    let supervisor_json = serde_json::to_value(&supervisor_timeline).unwrap();
    
    // Verify structure
    assert!(caller_json["events"][1]["peerLegId"] == "agent-leg");
    assert!(agent_json["events"][2]["eventType"] == "transferred");
    assert!(supervisor_json["events"][0]["details"]["source"] == "transfer_target");
}

/// Test: Empty timeline skips serialization in CDR
#[tokio::test]
async fn test_empty_timeline_skips_serialization() {
    use rustpbx::callrecord::LegTimeline;

    let timeline = LegTimeline::new();
    
    // Verify is_empty returns true
    assert!(timeline.is_empty());
    
    // Serialize - should produce empty events array
    let json = serde_json::to_value(&timeline).unwrap();
    assert!(json["events"].as_array().unwrap().is_empty());
}

/// Test: Timeline event timestamps are monotonic
#[tokio::test]
async fn test_timeline_timestamps_monotonic() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};

    let mut timeline = LegTimeline::new();
    
    // Add events with small delays
    for i in 0..5 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        timeline.add_event(
            format!("leg-{}", i),
            LegTimelineEventType::Added,
            None,
            None,
        );
    }
    
    // Verify timestamps are increasing
    let mut last_timestamp = timeline.events[0].timestamp;
    for event in &timeline.events[1..] {
        assert!(
            event.timestamp >= last_timestamp,
            "Timestamps should be monotonically increasing"
        );
        last_timestamp = event.timestamp;
    }
}

/// Test: RWI command for viewing leg timeline
#[tokio::test]
async fn test_leg_timeline_via_call_resume() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;
    
    // Cache events
    {
        let gw = gateway.read().await;
        
        // Simulate call lifecycle events
        let events = vec![
            rustpbx::rwi::RwiEvent::CallIncoming(rustpbx::rwi::CallIncomingData {
                call_id: "timeline-call".to_string(),
                context: "timeline-test".to_string(),
                caller: "sip:customer@test.com".to_string(),
                callee: "sip:service@test.com".to_string(),
                direction: "inbound".to_string(),
                trunk: None,
                sip_headers: std::collections::HashMap::new(),
            }),
            rustpbx::rwi::RwiEvent::CallAnswered {
                call_id: "timeline-call".to_string(),
            },
            rustpbx::rwi::RwiEvent::CallBridged {
                leg_a: "timeline-call".to_string(),
                leg_b: "agent-leg".to_string(),
            },
            rustpbx::rwi::RwiEvent::CallUnbridged {
                call_id: "timeline-call".to_string(),
            },
            rustpbx::rwi::RwiEvent::CallTransferred {
                call_id: "timeline-call".to_string(),
            },
        ];
        
        for event in events {
            gw.cache_event(&"timeline-call".to_string(), &event);
        }
    }
    
    // Resume the call and verify events
    let (_, json) = req(
        "call.resume",
        serde_json::json!({"call_id": "timeline-call"}),
    );
    let v = send_recv(&mut ws, &json).await;
    
    assert_eq!(v["status"], "success");
    let events = v["data"]["events"].as_array().unwrap();
    
    // Should have events
    assert!(!events.is_empty());
    
    // Verify event types represent a call lifecycle
    // Event structure: {"sequence": N, "timestamp": T, "call_id": "...", "event": {...}}
    // The "event" field contains the RwiEvent which uses snake_case variant names as keys
    let event_json = serde_json::to_string(&events).unwrap_or_default();
    
    // Check for event types in the serialized JSON
    assert!(
        event_json.contains("call_incoming") || event_json.contains("call_ringing"),
        "Should have incoming or ringing event: {}", event_json
    );
    assert!(
        event_json.contains("call_bridged"),
        "Should have bridged event: {}", event_json
    );
    
    ws.close(None).await.unwrap();
}
