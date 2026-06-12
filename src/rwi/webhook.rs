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
    
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct TestHttpServer { port: u16, received: Arc<Mutex<Vec<serde_json::Value>>> }
    impl TestHttpServer {
        async fn start() -> Self {
            let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
            let rc = received.clone();
            let app = axum::Router::new().route("/hook", axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                rc.lock().unwrap().push(body);
                async { axum::Json(serde_json::json!({"status":"ok"})) }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            crate::utils::spawn(async move { axum::serve(listener, app).await.ok(); });
            Self { port, received }
        }
        fn url(&self) -> String { format!("http://127.0.0.1:{}/hook", self.port) }
    }

    async fn wait_for_events(received: &Arc<Mutex<Vec<serde_json::Value>>>, min: usize, ms: u64) {
        let start = std::time::Instant::now();
        loop {
            if received.lock().unwrap().len() >= min { return; }
            if start.elapsed() > Duration::from_millis(ms) { return; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_webhook_receives_call_ringing() {
        let server = TestHttpServer::start().await;
        let config = LocatorWebhookConfig { url: server.url(), events: vec![], headers: None, timeout_ms: Some(5000) };
        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let entry = EventCacheEntry { sequence: 1, cached_at: chrono::Utc::now(), call_id: "c1".into(),
            event: crate::rwi::event::to_legacy_event(&crate::rwi::CallRinging { call_id: "c1".into() }, None) };
        tx.send(entry).ok();
        wait_for_events(&server.received, 1, 2000).await;
        let body = &server.received.lock().unwrap()[0];
        assert_eq!(body["event_type"], "call_ringing");
    }
}
