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
        // Events with empty call_id (broadcast events like agent state changes)
        // are not deduped since they have no call context and always use sequence=0.
        if !entry.call_id.is_empty() {
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
        }

        // Determine the RWI event type name and flat value from the enum variant.
        let event_value = &entry.event.payload;
        let event_type = entry.event.event_type;

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

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct TestHttpServer {
        port: u16,
        received: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl TestHttpServer {
        async fn start() -> Self {
            let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
            let rc = received.clone();
            let app = axum::Router::new().route(
                "/hook",
                axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    rc.lock().unwrap().push(body);
                    async { axum::Json(serde_json::json!({"status":"ok"})) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    async fn wait_for_events(received: &Arc<Mutex<Vec<serde_json::Value>>>, min: usize, ms: u64) {
        let start = std::time::Instant::now();
        loop {
            if received.lock().unwrap().len() >= min {
                return;
            }
            if start.elapsed() > Duration::from_millis(ms) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_webhook_receives_call_ringing() {
        let server = TestHttpServer::start().await;
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
            call_id: "c1".into(),
            event: crate::rwi::event::to_legacy_event(
                &crate::rwi::CallRinging {
                    call_id: "c1".into(),
                },
                None,
            ),
        };
        tx.send(entry).ok();
        wait_for_events(&server.received, 1, 2000).await;
        let body = &server.received.lock().unwrap()[0];
        assert_eq!(body["event_type"], "call_ringing");
    }

    /// Regression: agent status, recording metadata, and recording finalization
    /// events must all be deliverable through the RWI webhook. These three event
    /// types are the ones most commonly missing because of a stale `events`
    /// allow-list (the docs used to suggest `dn_state_changed`, which no longer
    /// exists, and omitted the recording-data events).
    #[tokio::test]
    async fn test_webhook_receives_agent_and_recording_events() {
        let server = TestHttpServer::start().await;
        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let now = chrono::Utc::now();

        // agent_state_changed: broadcast-style event (empty call_id, sequence=0,
        // intentionally NOT deduped by the handler).
        let agent_entry = EventCacheEntry {
            sequence: 0,
            cached_at: now,
            call_id: String::new(),
            event: crate::rwi::event::RwiEvent {
                event_type: "agent_state_changed",
                call_id: None,
                payload: serde_json::json!({
                    "event_type": "agent_state_changed",
                    "agent_id": "agent-1",
                    "from_status": "offline",
                    "to_status": "idle",
                }),
            },
        };
        // recording_metadata_available: carries the download URL after upload.
        let rec_meta_entry = EventCacheEntry {
            sequence: 100,
            cached_at: now,
            call_id: "call-1".into(),
            event: crate::rwi::event::RwiEvent {
                event_type: "recording_metadata_available",
                call_id: Some("call-1".into()),
                payload: serde_json::json!({
                    "event_type": "recording_metadata_available",
                    "call_id": "call-1",
                    "metadata": { "download_url": "https://example.com/rec.wav" },
                }),
            },
        };
        // record_end: recording finalization (url/duration/file_size).
        let record_end_entry = EventCacheEntry {
            sequence: 101,
            cached_at: now,
            call_id: "call-1".into(),
            event: crate::rwi::event::RwiEvent {
                event_type: "record_end",
                call_id: Some("call-1".into()),
                payload: serde_json::json!({
                    "event_type": "record_end",
                    "call_id": "call-1",
                    "url": "https://example.com/rec.wav",
                    "duration_secs": 12,
                    "file_size": 1024,
                }),
            },
        };

        tx.send(agent_entry).ok();
        tx.send(rec_meta_entry).ok();
        tx.send(record_end_entry).ok();

        wait_for_events(&server.received, 3, 2000).await;

        let received = server.received.lock().unwrap();
        let types: Vec<String> = received
            .iter()
            .map(|v| v["event_type"].as_str().unwrap().to_string())
            .collect();
        assert!(
            types.contains(&"agent_state_changed".to_string()),
            "agent_state_changed should be delivered via webhook: {types:?}"
        );
        assert!(
            types.contains(&"recording_metadata_available".to_string()),
            "recording_metadata_available should be delivered via webhook: {types:?}"
        );
        assert!(
            types.contains(&"record_end".to_string()),
            "record_end should be delivered via webhook: {types:?}"
        );
    }
}
