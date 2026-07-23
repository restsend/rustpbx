use crate::config::LocatorWebhookConfig;
use crate::rwi::gateway::EventCacheEntry;
use anyhow::anyhow;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Buffer size for the broadcast channel between gateway and webhook handler.
const WEBHOOK_CHANNEL_SIZE: usize = 512;
/// Max number of recent event (call_id, sequence) pairs kept for dedup.
const DEDUP_CACHE_SIZE: usize = 4096;

struct RwiWebhookSender {
    url: String,
    headers: std::collections::HashMap<String, String>,
    allowed_events: Vec<String>,
    client: reqwest::Client,
}

impl RwiWebhookSender {
    fn new(config: LocatorWebhookConfig) -> Self {
        let timeout = std::time::Duration::from_millis(config.timeout_ms.unwrap_or(5000));
        Self {
            url: config.url.trim().to_string(),
            headers: config.headers.unwrap_or_default(),
            allowed_events: config.events,
            client: crate::http_util::build_keepalive_client(Some(timeout), None)
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    fn accepts_event(&self, event_type: &str) -> bool {
        self.allowed_events.is_empty() || self.allowed_events.iter().any(|e| e == event_type)
    }

    /// Deliver the payload to the configured webhook URL, returning a record
    /// describing the call (url, status code, latency, body preview) for
    /// structured logging. The request is sent directly (rather than via
    /// `http_util::execute_request`) so that the HTTP status code is captured
    /// for *every* response — including non-2xx — which is essential for
    /// observability. The body is truncated to keep logs bounded.
    async fn send_payload(
        &self,
        payload: &serde_json::Value,
    ) -> Result<WebhookCallRecord, anyhow::Error> {
        let start = std::time::Instant::now();
        let mut req = self.client.post(&self.url).json(payload);
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        // The client is built with a connect/read timeout, so we don't wrap
        // an additional timeout here.
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("HTTP request failed: {}", e))?;
        let status_code = resp.status().as_u16();
        // Drain the body (best-effort) so the connection can be reused.
        let _ = resp.text().await;
        Ok(WebhookCallRecord {
            url: self.url.clone(),
            status_code: Some(status_code),
            latency_ms: start.elapsed().as_millis() as u64,
            body_preview: truncate_payload(payload),
        })
    }
}

/// Captured metadata for a single webhook delivery attempt, used for
/// structured observability logging.
#[derive(Debug, Clone)]
pub struct WebhookCallRecord {
    pub url: String,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    pub body_preview: String,
}

/// Truncate a JSON payload to a bounded preview for logging.
fn truncate_payload(payload: &serde_json::Value) -> String {
    const MAX_BODY_PREVIEW: usize = 1024;
    let s = payload.to_string();
    if s.len() <= MAX_BODY_PREVIEW {
        s
    } else {
        format!("{}…(truncated {} bytes)", &s[..MAX_BODY_PREVIEW], s.len())
    }
}

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
    let sender = RwiWebhookSender::new(config);
    debug!("RWI webhook handler started for {}", sender.url);

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
        if !sender.accepts_event(event_type) {
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

        match sender.send_payload(&payload).await {
            Ok(record) => {
                let success = record
                    .status_code
                    .map(|c| (200..300).contains(&c))
                    .unwrap_or(false);
                let call_id = if entry.call_id.is_empty() {
                    "-"
                } else {
                    entry.call_id.as_str()
                };
                if success {
                    info!(
                        url = %record.url,
                        event_type,
                        call_id,
                        sequence = entry.sequence,
                        status_code = record.status_code.unwrap_or(0),
                        latency_ms = record.latency_ms,
                        "RWI webhook delivered"
                    );
                } else {
                    warn!(
                        url = %record.url,
                        event_type,
                        call_id,
                        sequence = entry.sequence,
                        status_code = record.status_code.unwrap_or(0),
                        latency_ms = record.latency_ms,
                        body_preview = %record.body_preview,
                        "RWI webhook returned non-success status"
                    );
                }
            }
            Err(e) => {
                warn!(
                    url = %sender.url,
                    event_type,
                    call_id = %entry.call_id,
                    sequence = entry.sequence,
                    error = %e,
                    "RWI webhook send failed"
                );
            }
        }
    }
}

/// Send a test RWI event to a webhook URL.
pub async fn send_test_event(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
) -> Result<(), anyhow::Error> {
    let sender = RwiWebhookSender::new(LocatorWebhookConfig {
        url: url.to_string(),
        events: Vec::new(),
        headers: headers.cloned(),
        timeout_ms: Some(5000),
    });
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

    sender.send_payload(&test_payload).await.map(|_| ())
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

    /// `send_payload` returns a `WebhookCallRecord` capturing the response
    /// status code and latency, used for structured observability logging.
    #[tokio::test]
    async fn test_send_payload_captures_status_and_latency() {
        let server = TestHttpServer::start().await;
        let sender = RwiWebhookSender::new(LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        });

        let payload = json!({"event_type": "test", "call_id": "c1"});
        let record = sender.send_payload(&payload).await.expect("send ok");

        assert_eq!(record.url, server.url());
        assert_eq!(record.status_code, Some(200));
        assert!(record.latency_ms < 5000, "latency should be bounded");
        assert!(
            record.body_preview.contains("test"),
            "body preview should reflect payload"
        );
    }

    /// Non-success responses are still captured (status code recorded) so the
    /// dispatch loop can log them at warn.
    #[tokio::test]
    async fn test_send_payload_captures_non_success_status() {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(|| async {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let sender = RwiWebhookSender::new(LocatorWebhookConfig {
            url: format!("http://127.0.0.1:{}/hook", port),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        });

        let payload = json!({"event_type": "test"});
        // send_payload treats any HTTP response as Ok (it only errors on
        // transport failure); the status code is captured in the record.
        let record = sender.send_payload(&payload).await.expect("http ok");
        assert_eq!(record.status_code, Some(500));
    }

    /// Body preview is truncated for very large payloads to keep logs bounded.
    #[test]
    fn test_truncate_payload_bounds_size() {
        let huge = serde_json::Value::String("x".repeat(5000));
        let preview = truncate_payload(&huge);
        assert!(
            preview.len() < 5000,
            "preview must be truncated, len={}",
            preview.len()
        );
        assert!(preview.contains("truncated"));
    }
}
