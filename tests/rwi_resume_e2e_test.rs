// E2E tests for Event Replay & Recovery
// 
// These tests verify:
// 1. Session resume returns cached events after disconnect
// 2. Call resume returns call-specific events
// 3. Event sequence numbers are correctly tracked

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

const TEST_TOKEN: &str = "resume-test-token";

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

/// Send a request and wait for response matching the action_id (skipping events)
async fn send_recv_matching(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    json: &str,
    expected_action_id: &str,
) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    loop {
        let msg = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).expect("not JSON");
                // Check if this is the response we're looking for
                if v.get("action_id").and_then(|a| a.as_str()) == Some(expected_action_id) {
                    return v;
                }
                // Otherwise continue to next message (could be an event)
            }
            other => panic!("unexpected frame: {:?}", other),
        }
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
            let gw = gateway.read().await;
            let events = vec![
                rustpbx::rwi::RwiEvent::CallIncoming(rustpbx::rwi::CallIncomingData {
                    call_id: "resume-call-1".to_string(),
                    context: "resume-test".to_string(),
                    caller: "sip:alice@test.com".to_string(),
                    callee: "sip:bob@test.com".to_string(),
                    direction: "inbound".to_string(),
                    trunk: None,
                    sip_headers: std::collections::HashMap::new(),
                }),
                rustpbx::rwi::RwiEvent::CallRinging {
                    call_id: "resume-call-1".to_string(),
                },
                rustpbx::rwi::RwiEvent::CallAnswered {
                    call_id: "resume-call-1".to_string(),
                },
            ];
            
            for event in events {
                gw.fan_out_event_to_context("resume-test", &event, &"resume-call-1".to_string());
            }
        }
        
        // Give time for events to be cached
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Get current sequence before disconnect
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
        
        // Resume with no last_sequence (should get all cached events)
        let (_, json) = req("session.resume", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        
        assert_eq!(v["status"], "success");
        let replayed_count = v["data"]["replayed_count"].as_u64().unwrap();
        let current_sequence = v["data"]["current_sequence"].as_u64().unwrap();
        
        // Should have at least the events we pushed
        assert!(
            replayed_count >= session_events,
            "Should replay at least {} events, got {}",
            session_events,
            replayed_count
        );
        
        // Current sequence should be >= replayed count
        assert!(
            current_sequence >= replayed_count,
            "Current sequence ({}) should be >= replayed count ({})",
            current_sequence,
            replayed_count
        );
        
        // Verify event structure
        let events = v["data"]["events"].as_array().unwrap();
        assert!(!events.is_empty(), "Should have events");
        
        // Verify each event has required fields
        for event in events {
            assert!(event["sequence"].is_u64(), "Event should have sequence");
            assert!(event["timestamp"].is_u64(), "Event should have timestamp");
            assert!(event["call_id"].is_string(), "Event should have call_id");
            assert!(event["event"].is_object(), "Event should have event data");
        }
        
        ws.close(None).await.unwrap();
    }
}

/// Test: Incremental resume with last_sequence
/// 1. Cache multiple events
/// 2. Resume from sequence 0 (should get all)
/// 3. Resume from middle sequence (should get partial)
#[tokio::test]
async fn test_incremental_resume_with_sequence() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;
    
    // Push events to gateway cache
    {
        let gw = gateway.read().await;
        for i in 0..5 {
            let event = rustpbx::rwi::RwiEvent::CallRinging {
                call_id: format!("seq-call-{}", i),
            };
            gw.cache_event(&format!("seq-call-{}", i), &event);
        }
    }
    
    // Resume without sequence (get all)
    let (_, json) = req("session.resume", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    let total_count = v["data"]["replayed_count"].as_u64().unwrap();
    
    // Resume from sequence 2 (should get events after sequence 2)
    let (_, json) = req("session.resume", serde_json::json!({"last_sequence": 2}));
    let v = send_recv(&mut ws, &json).await;
    let partial_count = v["data"]["replayed_count"].as_u64().unwrap();
    
    // Partial count should be less than total
    assert!(
        partial_count < total_count,
        "Partial resume should return fewer events: {} < {}",
        partial_count,
        total_count
    );
    
    ws.close(None).await.unwrap();
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
        let gw = gateway.read().await;
        
        // Call A events
        gw.cache_event(&"call-a".to_string(), &rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "call-a".to_string(),
        });
        gw.cache_event(&"call-a".to_string(), &rustpbx::rwi::RwiEvent::CallAnswered {
            call_id: "call-a".to_string(),
        });
        
        // Call B events
        gw.cache_event(&"call-b".to_string(), &rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "call-b".to_string(),
        });
        gw.cache_event(&"call-b".to_string(), &rustpbx::rwi::RwiEvent::CallBridged {
            leg_a: "call-b".to_string(),
            leg_b: "call-c".to_string(),
        });
    }
    
    // Resume call-a specifically
    let (_, json) = req(
        "call.resume",
        serde_json::json!({"call_id": "call-a"}),
    );
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
    let (_, json) = req(
        "call.resume",
        serde_json::json!({"call_id": "call-b"}),
    );
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
    assert!(events.is_empty(), "Should have no events for non-existent call");
    assert_eq!(v["data"]["replayed_count"], 0);
    
    ws.close(None).await.unwrap();
}

/// Test: Event sequence monotonicity
/// Verify that sequence numbers are monotonically increasing
#[tokio::test]
async fn test_event_sequence_monotonicity() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;
    
    // Push events sequentially
    {
        let gw = gateway.read().await;
        for i in 0..10 {
            let event = rustpbx::rwi::RwiEvent::Dtmf {
                call_id: "dtmf-call".to_string(),
                digit: i.to_string(),
            };
            gw.cache_event(&"dtmf-call".to_string(), &event);
        }
    }
    
    // Resume and check sequence numbers
    let (_, json) = req("call.resume", serde_json::json!({"call_id": "dtmf-call"}));
    let v = send_recv(&mut ws, &json).await;
    
    let events = v["data"]["events"].as_array().unwrap();
    assert!(!events.is_empty());
    
    // Check that sequences are strictly increasing
    let mut last_seq: u64 = 0;
    for event in events {
        let seq = event["sequence"].as_u64().unwrap();
        assert!(
            seq > last_seq,
            "Sequence {} should be greater than {}",
            seq,
            last_seq
        );
        last_seq = seq;
    }
    
    ws.close(None).await.unwrap();
}
