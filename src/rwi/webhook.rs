use crate::config::LocatorWebhookConfig;
use crate::rwi::gateway::EventCacheEntry;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Buffer size for the broadcast channel between gateway and webhook handler.
const WEBHOOK_CHANNEL_SIZE: usize = 512;
/// Max number of recent event (call_id, sequence) pairs kept for dedup.
const DEDUP_CACHE_SIZE: usize = 4096;

/// Start the RWI webhook handler background task.
///
/// Returns a `broadcast::Sender` that the gateway can use to send events.
pub fn start_rwi_webhook_handler(
    config: LocatorWebhookConfig,
) -> broadcast::Sender<EventCacheEntry> {
    let (tx, rx) = broadcast::channel(WEBHOOK_CHANNEL_SIZE);
    crate::utils::spawn(run_rwi_webhook_handler(config, rx));
    tx
}

async fn run_rwi_webhook_handler(
    config: LocatorWebhookConfig,
    mut rx: broadcast::Receiver<EventCacheEntry>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(
            config.timeout_ms.unwrap_or(5000),
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let url = config.url.trim().to_string();
    debug!("RWI webhook handler started for {}", url);

    // Dedup cache: ring buffer of (call_id, sequence) to skip duplicates
    // when the same event is forwarded from multiple call owners.
    let mut dedup: VecDeque<(String, u64)> = VecDeque::with_capacity(DEDUP_CACHE_SIZE + 1);
    let mut seen: HashSet<(String, u64)> = HashSet::new();

    loop {
        let entry = match rx.recv().await {
            Ok(entry) => entry,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("RWI webhook lagged, missed {} events", n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        };

        // Dedup: skip event if same (call_id, sequence) already sent.
        let dedup_key = (entry.call_id.clone(), entry.sequence);
        if seen.contains(&dedup_key) {
            debug!("RWI webhook: skipping duplicate event {}", entry.sequence);
            continue;
        }
        seen.insert(dedup_key.clone());
        dedup.push_back(dedup_key);
        while dedup.len() > DEDUP_CACHE_SIZE {
            if let Some(old) = dedup.pop_front() {
                seen.remove(&old);
            }
        }

        // Determine the RWI event type name and flat value from the enum variant.
        let (event_value, event_type) = entry.event.to_flat_value();

        // Apply event type filter if configured.
        if !config.events.is_empty() && !config.events.iter().any(|e| e == event_type) {
            continue;
        }

        let payload = json!({
            "rwi": "1.0",
            "sequence": entry.sequence,
            "timestamp": entry.cached_at.to_rfc3339(),
            "call_id": entry.call_id,
            "event_type": event_type,
            "event": event_value,
        });

        let header_map = config.headers.clone().unwrap_or_default();
        let req = client.post(&url).json(&payload);
        if let Err(e) = crate::http_util::execute_request(req, &header_map, None).await {
            warn!(
                "RWI webhook send failed for {} (event: {}): {}",
                url, event_type, e
            );
        }
    }
}



/// Send a test RWI event to a webhook URL.
pub async fn send_test_event(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
) -> Result<(), anyhow::Error> {
    let test_payload = json!({
        "rwi": "1.0",
        "sequence": 0,
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "call_id": "test-call-id",
        "event_type": "test",
        "event": {
            "test": {
                "message": "RustPBX RWI webhook test"
            }
        }
    });

    let opts = crate::http_util::HttpFetchOptions::new()
        .with_timeout(std::time::Duration::from_secs(5))
        .with_headers(headers.cloned().unwrap_or_default());

    let req = reqwest::Client::new().post(url).json(&test_payload);
    crate::http_util::execute_request(req, &opts.headers, opts.timeout).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LocatorWebhookConfig;
    use crate::rwi::gateway::{EventCacheEntry, RwiGateway};
    use crate::rwi::proto::{CallIncomingData, RwiEvent};
    use axum::{Json, Router, routing::post};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    struct TestHttpServer {
        port: u16,
        received: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl TestHttpServer {
        async fn start() -> Self {
            let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
            let received_clone = received.clone();

            let app = Router::new().route(
                "/hook",
                post(move |Json(body): Json<serde_json::Value>| {
                    received_clone.lock().unwrap().push(body);
                    async { Json(serde_json::json!({"status": "ok"})) }
                }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("failed to bind test server");
            let port = listener.local_addr().unwrap().port();

            crate::utils::spawn(async move {
                axum::serve(listener, app).await.ok();
            });

            Self { port, received }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/hook", self.port)
        }
    }

    async fn wait_for_events(
        received: &Arc<Mutex<Vec<serde_json::Value>>>,
        min_count: usize,
        timeout_ms: u64,
    ) {
        let start = std::time::Instant::now();
        loop {
            let count = received.lock().unwrap().len();
            if count >= min_count {
                return;
            }
            if start.elapsed() > Duration::from_millis(timeout_ms) {
                panic!(
                    "timeout waiting for {min_count} events, got {count} after {}ms",
                    start.elapsed().as_millis()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_webhook_receives_call_ringing_event() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = EventCacheEntry {
            sequence: 1,
            cached_at: chrono::Utc::now(),
            call_id: "test-call-1".into(),
            event: RwiEvent::CallRinging {
                call_id: "test-call-1".into(),
                context: Default::default(),
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 1, "expected exactly one event");
        let body = &received[0];
        assert_eq!(body["rwi"], "1.0");
        assert_eq!(body["event_type"], "call_ringing");
        assert_eq!(body["call_id"], "test-call-1");
        assert_eq!(body["sequence"], 1);
        assert!(body["event"]["call_id"].is_string());
    }

    #[tokio::test]
    async fn test_webhook_receives_call_hangup_event() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = EventCacheEntry {
            sequence: 10,
            cached_at: chrono::Utc::now(),
            call_id: "test-call-2".into(),
            event: RwiEvent::CallHangup {
                call_id: "test-call-2".into(),
                reason: Some("normal_clearing".into()),
                sip_status: Some(200),
                context: Default::default(),
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        let body = &received[0];
        assert_eq!(body["event_type"], "call_hangup");
        assert_eq!(body["call_id"], "test-call-2");
        assert_eq!(body["event"]["reason"], "normal_clearing");
        assert_eq!(body["event"]["sip_status"], 200);
    }

    #[tokio::test]
    async fn test_webhook_receives_dn_state_changed_event() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = EventCacheEntry {
            sequence: 99,
            cached_at: chrono::Utc::now(),
            call_id: "call-dn-1".into(),
            event: RwiEvent::DnStateChanged {
                caller: "80001".into(),
                event_name: "ESTABLISHED".into(),
                system_time: "2026-05-14T17:54:49.003Z".into(),
                call_id: Some("call-dn-1".into()),
                agent_id: Some("10001".into()),
                caller_name: Some("19534519769".into()),
                callee_name: Some("39989".into()),
                reason_code: None,
                agent_work_mode: None,
                releasing_party: None,
                vq_name: None,
                routing_target: None,
                skill_group: None,
                extra: None,
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        let body = &received[0];
        assert_eq!(body["event_type"], "dn_state_changed");
        assert_eq!(body["event"]["event_name"], "ESTABLISHED");
    }

    #[tokio::test]
    async fn test_webhook_event_filtering() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec!["call_ringing".into()],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send hangup first (should be filtered OUT)
        tx.send(EventCacheEntry {
            sequence: 1,
            cached_at: chrono::Utc::now(),
            call_id: "c-1".into(),
            event: RwiEvent::CallHangup {
                call_id: "c-1".into(),
                reason: None,
                sip_status: None,
                context: Default::default(),
            },
        })
        .ok();

        // Send ringing (should be delivered)
        tx.send(EventCacheEntry {
            sequence: 2,
            cached_at: chrono::Utc::now(),
            call_id: "c-2".into(),
            event: RwiEvent::CallRinging {
                call_id: "c-2".into(),
                context: Default::default(),
            },
        })
        .ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 1, "only call_ringing should pass filter");
        assert_eq!(received[0]["event_type"], "call_ringing");
    }

    #[tokio::test]
    async fn test_webhook_multiple_events_ordering() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(EventCacheEntry {
            sequence: 1,
            cached_at: chrono::Utc::now(),
            call_id: "c-1".into(),
            event: RwiEvent::CallRinging {
                call_id: "c-1".into(),
                context: Default::default(),
            },
        })
        .ok();
        tx.send(EventCacheEntry {
            sequence: 2,
            cached_at: chrono::Utc::now(),
            call_id: "c-2".into(),
            event: RwiEvent::CallAnswered {
                call_id: "c-2".into(),
                context: Default::default(),
            },
        })
        .ok();
        tx.send(EventCacheEntry {
            sequence: 3,
            cached_at: chrono::Utc::now(),
            call_id: "c-3".into(),
            event: RwiEvent::CallHangup {
                call_id: "c-3".into(),
                reason: Some("normal".into()),
                sip_status: Some(200),
                context: Default::default(),
            },
        })
        .ok();

        wait_for_events(&server.received, 3, 3000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 3);
        assert_eq!(received[0]["sequence"], 1);
        assert_eq!(received[0]["event_type"], "call_ringing");
        assert_eq!(received[1]["sequence"], 2);
        assert_eq!(received[1]["event_type"], "call_answered");
        assert_eq!(received[2]["sequence"], 3);
        assert_eq!(received[2]["event_type"], "call_hangup");
    }

    #[tokio::test]
    async fn test_webhook_receives_from_gateway_broadcast() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut gateway = RwiGateway::new();
        gateway.set_webhook_tx(tx);

        gateway.broadcast_event(&RwiEvent::CallRinging {
            call_id: "gw-test-call".into(),
            context: Default::default(),
        });

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 1, "expected one event from gateway");
        assert_eq!(received[0]["event_type"], "call_ringing");
        assert_eq!(received[0]["call_id"], ""); // broadcast uses empty call_id
    }

    #[tokio::test]
    async fn test_webhook_dedup_same_event_twice() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send the same event twice with identical (call_id, sequence).
        let entry = EventCacheEntry {
            sequence: 42,
            cached_at: chrono::Utc::now(),
            call_id: "dedup-test".into(),
            event: RwiEvent::CallRinging {
                call_id: "dedup-test".into(),
                context: Default::default(),
            },
        };
        tx.send(entry.clone()).ok();
        tx.send(entry).ok();

        // Only one copy should arrive.
        wait_for_events(&server.received, 1, 2000).await;
        tokio::time::sleep(Duration::from_millis(200)).await; // give time for any spurious duplicate

        let received = server.received.lock().unwrap();
        assert_eq!(
            received.len(),
            1,
            "duplicate event should be suppressed, got {}",
            received.len()
        );
        assert_eq!(received[0]["sequence"], 42);
    }

    #[tokio::test]
    async fn test_webhook_receives_multiple_call_events_via_send_to_owner() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut gateway = RwiGateway::new();
        gateway.set_webhook_tx(tx);

        let call_id = "e2e-call".to_string();

        // 1. Send CallIncoming via send_event_to_call_owner (same path as emit_rwi_event)
        gateway.send_event_to_call_owner(
            &call_id,
            &RwiEvent::CallIncoming(CallIncomingData {
                call_id: call_id.clone(),
                context: "default".into(),
                caller: "alice".into(),
                callee: "ivr".into(),
                dial_direction: "inbound".into(),
                trunk: None,
                sip_headers: HashMap::new(),
                root_call_id: None,
                caller_name: None,
                callee_name: None,
                called_phone: None,
                app_id: None,
                routing_target: None,
                uuid: None,
                routing_path: None,
            }),
        );

        // 2. Send CallRinging
        gateway.send_event_to_call_owner(&call_id, &RwiEvent::ringing(call_id.clone()));

        // 3. Send CallAnswered
        gateway.send_event_to_call_owner(&call_id, &RwiEvent::answered(call_id.clone()));

        // 4. Send CallHangup
        gateway.send_event_to_call_owner(
            &call_id,
            &RwiEvent::hangup(call_id.clone(), Some("ByCaller".into()), Some(200)),
        );

        // All 4 events should arrive with correct call_id and unique sequences
        wait_for_events(&server.received, 4, 3000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(
            received.len(),
            4,
            "expected 4 events, got {}",
            received.len()
        );

        // Verify each event type and call_id
        let event_types: Vec<String> = received
            .iter()
            .map(|v| v["event_type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            event_types,
            vec![
                "call_incoming",
                "call_ringing",
                "call_answered",
                "call_hangup",
            ]
        );

        // Verify all have the correct call_id (not empty)
        for v in received.iter() {
            assert_eq!(
                v["call_id"], call_id,
                "call_id mismatch for event {}",
                v["event_type"]
            );
        }

        // Verify sequences are unique and in order
        let sequences: Vec<u64> = received
            .iter()
            .map(|v| v["sequence"].as_u64().unwrap())
            .collect();
        assert_eq!(sequences, vec![1, 2, 3, 4], "sequences should be 1..4");
    }

    #[tokio::test]
    async fn test_webhook_dedup_does_not_swallow_unique_events() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Two different events with different (call_id, sequence) — both must arrive.
        tx.send(EventCacheEntry {
            sequence: 1,
            cached_at: chrono::Utc::now(),
            call_id: "c-1".into(),
            event: RwiEvent::CallRinging {
                call_id: "c-1".into(),
                context: Default::default(),
            },
        })
        .ok();
        tx.send(EventCacheEntry {
            sequence: 2,
            cached_at: chrono::Utc::now(),
            call_id: "c-2".into(),
            event: RwiEvent::CallHangup {
                call_id: "c-2".into(),
                reason: None,
                sip_status: None,
                context: Default::default(),
            },
        })
        .ok();

        wait_for_events(&server.received, 2, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0]["sequence"], 1);
        assert_eq!(received[1]["sequence"], 2);
    }
}
