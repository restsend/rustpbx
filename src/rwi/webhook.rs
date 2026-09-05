use crate::config::LocatorWebhookConfig;
use crate::rwi::gateway::EventCacheEntry;
use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Buffer size for the broadcast channel between gateway and webhook handler.
const WEBHOOK_CHANNEL_SIZE: usize = 512;
/// Max number of recent (call_id, timestamp) pairs kept for dedup.
const DEDUP_CACHE_SIZE: usize = 4096;
/// Idempotent retry policy: a failed delivery (transport error or non-2xx
/// status) is retried up to [`WEBHOOK_RETRY_COUNT`] times with
/// [`WEBHOOK_RETRY_INTERVAL_MS`] between attempts. Every attempt re-sends the
/// byte-identical payload (same `event_id`), so receivers can safely dedupe.
const WEBHOOK_RETRY_COUNT: u32 = 3;
const WEBHOOK_RETRY_INTERVAL_MS: u64 = 500;

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
    /// describing the call (url, status code, latency) for structured logging.
    /// The request is sent directly (rather than via
    /// `http_util::execute_request`) so that the HTTP status code is captured
    /// for *every* response — including non-2xx — which is essential for
    /// observability. `body` is the pre-serialized payload; it is echoed into
    /// the record verbatim (never truncated) so log lines carry the complete
    /// event as a compensation record, and reused across retries so every
    /// attempt is byte-identical.
    async fn send_payload(
        &self,
        payload: &serde_json::Value,
        body: &str,
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
            body: body.to_string(),
        })
    }
}

/// Captured metadata for a single webhook delivery attempt, used for
/// structured observability logging. `body` carries the *complete* request
/// payload — never truncated — so the sender-side log can serve as a
/// compensation record when the receiver misses an event.
#[derive(Debug, Clone)]
pub struct WebhookCallRecord {
    pub url: String,
    pub status_code: Option<u16>,
    pub latency_ms: u64,
    pub body: String,
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

/// Dedup identity for webhook delivery retries/redeliveries.
///
/// Includes the event TYPE: `cached_at` has microsecond resolution, so two
/// DISTINCT events for the same call can legitimately share one timestamp
/// (observed in e2e: `call_created` vs `queue_joined` at session start) —
/// a key without the type silently dropped one of them.
fn webhook_dedup_key(
    entry: &EventCacheEntry,
) -> (String, DateTime<Utc>, String) {
    (
        entry.call_id.clone(),
        entry.cached_at,
        entry.event.event_type.to_string(),
    )
}

async fn run_rwi_webhook_handler(
    config: LocatorWebhookConfig,
    mut rx: broadcast::Receiver<EventCacheEntry>,
) {
    let sender = RwiWebhookSender::new(config);
    debug!("RWI webhook handler started for {}", sender.url);

    // Dedup cache: ring buffer of (call_id, cached_at) to skip duplicates
    // when the same event is forwarded from multiple call owners.
    let mut dedup: VecDeque<(String, DateTime<Utc>, String)> =
        VecDeque::with_capacity(DEDUP_CACHE_SIZE + 1);
    let mut seen: HashSet<(String, DateTime<Utc>, String)> = HashSet::new();
    // Consecutive transport-failure counter — used only to log a single
    // "recovered" line when delivery succeeds again after an outage.
    let mut consecutive_send_failures: u32 = 0;

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

        // Dedup: skip event if the same (call_id, timestamp, event_type) was
        // already sent. The timestamp has microsecond resolution, so distinct
        // events for the same call CAN share one — the event type keeps the
        // key from collapsing them (e.g. call_created vs queue_joined emitted
        // back-to-back at session start). Events with empty call_id
        // (broadcast events like agent state changes) are not deduped since
        // they have no call context.
        if !entry.call_id.is_empty() {
            let dedup_key = webhook_dedup_key(&entry);
            if seen.contains(&dedup_key) {
                debug!(
                    "RWI webhook: skipping duplicate event at {}",
                    entry.cached_at
                );
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
            // Idempotency key: re-sent unchanged on every retry attempt so
            // receivers can dedupe redeliveries.
            "event_id": uuid::Uuid::new_v4().to_string(),
            "timestamp": entry.cached_at.to_rfc3339(),
            "call_id": entry.call_id,
            "event_type": event_type,
            "event": event_value,
        });
        // Serialize once and reuse the exact bytes for every attempt and log
        // line. The body is never truncated: the full payload in the log
        // serves as a compensation record when the receiver misses an event.
        let body = payload.to_string();

        let total_attempts = 1 + WEBHOOK_RETRY_COUNT;
        for attempt in 1..=total_attempts {
            if attempt > 1 {
                tokio::time::sleep(Duration::from_millis(WEBHOOK_RETRY_INTERVAL_MS)).await;
            }
            match sender.send_payload(&payload, &body).await {
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
                        if consecutive_send_failures > 0 {
                            info!(
                                url = %record.url,
                                consecutive_failures = consecutive_send_failures,
                                "RWI webhook delivery recovered"
                            );
                        }
                        consecutive_send_failures = 0;
                        info!(
                            url = %record.url,
                            event_type,
                            call_id,
                            attempt,
                            status_code = record.status_code.unwrap_or(0),
                            latency_ms = record.latency_ms,
                            body = %record.body,
                            "RWI webhook delivered"
                        );
                        break;
                    }
                    if attempt < total_attempts {
                        warn!(
                            url = %record.url,
                            event_type,
                            call_id,
                            attempt,
                            max_attempts = total_attempts,
                            status_code = record.status_code.unwrap_or(0),
                            latency_ms = record.latency_ms,
                            "RWI webhook returned non-success status, retrying"
                        );
                        continue;
                    }
                    consecutive_send_failures += 1;
                    warn!(
                        url = %record.url,
                        event_type,
                        call_id,
                        attempts = total_attempts,
                        status_code = record.status_code.unwrap_or(0),
                        latency_ms = record.latency_ms,
                        body = %record.body,
                        "RWI webhook returned non-success status, giving up"
                    );
                }
                Err(e) => {
                    if attempt < total_attempts {
                        warn!(
                            url = %sender.url,
                            event_type,
                            call_id = %entry.call_id,
                            attempt,
                            max_attempts = total_attempts,
                            error = %e,
                            "RWI webhook send failed, retrying"
                        );
                        continue;
                    }
                    consecutive_send_failures += 1;
                    // INFO with the full request body: when the receiver is
                    // down this log is the only place to see which events
                    // (and payloads) were generated. The body is never
                    // truncated, so the log doubles as a compensation record.
                    info!(
                        url = %sender.url,
                        event_type,
                        call_id = %entry.call_id,
                        attempts = total_attempts,
                        body = %body,
                        error = %e,
                        "RWI webhook send failed"
                    );
                }
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
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "call_id": "test-call-id",
        "event_type": "test",
        "event": {
            "test": {
                "message": "RustPBX RWI webhook test"
            }
        }
    });

    sender
        .send_payload(&test_payload, &test_payload.to_string())
        .await
        .map(|_| ())
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
            cached_at: chrono::Utc::now(),
            call_id: "c1".into(),
            event: crate::rwi::event::to_legacy_event(
                &crate::rwi::CallRinging {
                    call_id: "c1".into(),
                    early_media: true,
                },
                None,
            ),
        };
        tx.send(entry).ok();
        wait_for_events(&server.received, 1, 2000).await;
        let body = &server.received.lock().unwrap()[0];
        assert_eq!(body["event_type"], "call_ringing");
        assert_eq!(
            body["event"]["early_media"].as_bool(),
            Some(true),
            "call_ringing must carry the early_media flag through the webhook"
        );
    }

    /// Regression canary for the same-microsecond dedup collapse: two
    /// DISTINCT event types for the same call sharing one `cached_at` must
    /// BOTH be delivered. The pre-fix key `(call_id, cached_at)` dropped the
    /// second one — e2e saw ~1/3 of `call_created` bindings lost this way.
    #[tokio::test]
    async fn test_webhook_dedup_same_timestamp_different_type_both_delivered() {
        let server = TestHttpServer::start().await;
        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let ts = chrono::Utc::now();
        let mk = |event: crate::rwi::event::RwiEvent| EventCacheEntry {
            cached_at: ts,
            call_id: "dedup-call".into(),
            event,
        };
        let ringing = mk(crate::rwi::event::to_legacy_event(
            &crate::rwi::CallRinging {
                call_id: "dedup-call".into(),
                early_media: false,
            },
            None,
        ));
        let created = mk(crate::rwi::event::to_legacy_event(
            &crate::rwi::CallCreated {
                call_id: "dedup-call".into(),
                context: "default".into(),
                caller: "sip:1001@localhost".into(),
                callee: "sip:8888@localhost".into(),
                trunk: None,
                sip_headers: Default::default(),
                caller_name: None,
                callee_name: None,
                called_phone: None,
                app_id: None,
                routing_target: None,
                uuid: None,
                routing_path: None,
            },
            None,
        ));
        tx.send(ringing).ok();
        tx.send(created).ok();

        wait_for_events(&server.received, 2, 2000).await;
        let received = server.received.lock().unwrap();
        let types: Vec<&str> = received
            .iter()
            .map(|b| b["event_type"].as_str().unwrap_or(""))
            .collect();
        assert!(
            types.contains(&"call_ringing") && types.contains(&"call_created"),
            "same-timestamp distinct-type events must both be delivered, got {types:?}"
        );
    }

    /// True redeliveries (identical identity incl. event type) are still
    /// deduplicated exactly once.
    #[tokio::test]
    async fn test_webhook_dedup_identical_event_dropped_once() {
        let server = TestHttpServer::start().await;
        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let ts = chrono::Utc::now();
        let mk = || EventCacheEntry {
            cached_at: ts,
            call_id: "dedup-once".into(),
            event: crate::rwi::event::to_legacy_event(
                &crate::rwi::CallRinging {
                    call_id: "dedup-once".into(),
                    early_media: false,
                },
                None,
            ),
        };
        tx.send(mk()).ok();
        tx.send(mk()).ok();

        wait_for_events(&server.received, 1, 2000).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let count = server.received.lock().unwrap().len();
        assert_eq!(count, 1, "identical redelivery must be deduped to one");
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

        // agent_state_changed: broadcast-style event (empty call_id,
        // intentionally NOT deduped by the handler).
        let agent_entry = EventCacheEntry {
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
            cached_at: now + chrono::Duration::milliseconds(1),
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
            cached_at: now + chrono::Duration::milliseconds(2),
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
        let body = payload.to_string();
        let record = sender.send_payload(&payload, &body).await.expect("send ok");

        assert_eq!(record.url, server.url());
        assert_eq!(record.status_code, Some(200));
        assert!(record.latency_ms < 5000, "latency should be bounded");
        assert!(record.body.contains("test"), "body should reflect payload");
        assert_eq!(record.body, body, "body must be the full payload");
    }

    /// Regression (panic fix): payloads whose serialized form exceeds 1024
    /// bytes and contains multi-byte UTF-8 characters (e.g. Chinese) straddling
    /// the old truncation boundary must be handled without panicking. Bodies
    /// are never truncated — the full payload is echoed for log compensation.
    #[tokio::test]
    async fn test_send_payload_handles_large_multibyte_body() {
        let server = TestHttpServer::start().await;
        let sender = RwiWebhookSender::new(LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        });

        // 1800 bytes of '请' (3 bytes each): byte 1024 falls inside a char,
        // which panicked the old `&s[..1024]` slice.
        let payload = json!({"event_type": "test", "note": "请".repeat(600)});
        let body = payload.to_string();
        assert!(body.len() > 1024);

        let record = sender.send_payload(&payload, &body).await.expect("send ok");
        assert_eq!(record.status_code, Some(200));
        assert_eq!(record.body, body, "body must not be truncated");
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
        let body = payload.to_string();
        let record = sender.send_payload(&payload, &body).await.expect("http ok");
        assert_eq!(record.status_code, Some(500));
    }

    /// HTTP server that answers `fail_status` for the first `fail_first`
    /// requests, then 200. Records every request body for retry assertions.
    struct RetryTestServer {
        port: u16,
        received: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl RetryTestServer {
        async fn start(fail_first: u32, fail_status: axum::http::StatusCode) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
            let rc = received.clone();
            let counter = Arc::new(AtomicU32::new(0));
            let app = axum::Router::new().route(
                "/hook",
                axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let rc = rc.clone();
                    let counter = counter.clone();
                    async move {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        rc.lock().unwrap().push(body);
                        if n < fail_first {
                            (
                                fail_status,
                                axum::Json(serde_json::json!({"status": "error"})),
                            )
                        } else {
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({"status": "ok"})),
                            )
                        }
                    }
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

    fn retry_test_entry(call_id: &str) -> EventCacheEntry {
        EventCacheEntry {
            cached_at: chrono::Utc::now(),
            call_id: call_id.into(),
            event: crate::rwi::event::to_legacy_event(
                &crate::rwi::CallAnswered {
                    call_id: call_id.into(),
                },
                None,
            ),
        }
    }

    /// Failed deliveries (non-2xx) are retried up to `WEBHOOK_RETRY_COUNT`
    /// times with `WEBHOOK_RETRY_INTERVAL_MS` between attempts, and every
    /// attempt re-sends a byte-identical payload (stable `event_id`) so
    /// receivers can dedupe.
    #[tokio::test]
    async fn test_webhook_retries_until_success_with_identical_payload() {
        let server = RetryTestServer::start(2, axum::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(retry_test_entry("retry-1")).ok();

        // 2 initial failures + 1 successful retry = 3 requests.
        wait_for_events(&server.received, 3, 5000).await;
        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 3, "expected exactly 3 delivery attempts");
        assert_eq!(
            received[0], received[1],
            "retry attempts must carry identical payloads"
        );
        assert_eq!(
            received[1], received[2],
            "retry attempts must carry identical payloads"
        );
        assert!(
            received[0].get("event_id").is_some(),
            "payload must carry an event_id idempotency key"
        );
    }

    /// After the initial attempt plus all retries fail, the handler gives up
    /// (exactly `1 + WEBHOOK_RETRY_COUNT` requests) and stays alive so
    /// subsequent events are still delivered.
    #[tokio::test]
    async fn test_webhook_gives_up_after_max_retries_and_stays_alive() {
        let server = RetryTestServer::start(u32::MAX, axum::http::StatusCode::BAD_GATEWAY).await;
        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(retry_test_entry("retry-2")).ok();

        // 1 initial attempt + 3 retries = 4 requests.
        wait_for_events(&server.received, 4, 8000).await;
        // No 5th attempt may follow (wait longer than the retry interval).
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            server.received.lock().unwrap().len(),
            4,
            "delivery must stop after 1 attempt + 3 retries"
        );

        // The handler loop must still be alive for the next event.
        tx.send(retry_test_entry("retry-3")).ok();
        wait_for_events(&server.received, 5, 8000).await;
        assert!(server.received.lock().unwrap().len() >= 5);
    }

    /// Large multi-byte payloads flow through the whole handler untouched —
    /// end-to-end regression for the UTF-8 char-boundary panic: the receiver
    /// must get the complete, untruncated payload.
    #[tokio::test]
    async fn test_webhook_delivers_large_multibyte_payload_end_to_end() {
        let server = TestHttpServer::start().await;
        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let transcript = "请检查录音质量与通话摘要。".repeat(100);
        let entry = EventCacheEntry {
            cached_at: chrono::Utc::now(),
            call_id: "call-cn".into(),
            event: crate::rwi::event::RwiEvent {
                event_type: "recording_metadata_available",
                call_id: Some("call-cn".into()),
                payload: serde_json::json!({
                    "event_type": "recording_metadata_available",
                    "call_id": "call-cn",
                    "metadata": { "transcript": transcript },
                }),
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;
        let body = &server.received.lock().unwrap()[0];
        assert_eq!(
            body["event"]["metadata"]["transcript"], transcript,
            "payload must be delivered in full, without truncation"
        );
    }
}
