//! Shared helper: minimal axum HTTP server that captures incoming RWI webhook
//! POST payloads in an `Arc<Mutex<Vec<serde_json::Value>>>`.
//!
//! Previously duplicated in `rwi_queue_agent_webhook_e2e_test.rs` and
//! `webhook_agent_events_e2e_test.rs`. Deduplicated here so both (and future
//! webhook-targeted e2e tests) share one implementation.

use std::sync::{Arc, Mutex};

/// A simple HTTP server that listens on an ephemeral port and captures every
/// POST body sent to `/hook` in `received`. Call `start()` to spin it up,
/// use `url()` to get the `http://127.0.0.1:{port}/hook` address to configure
/// as the webhook target.
pub struct WebhookCapture {
    pub received: Arc<Mutex<Vec<serde_json::Value>>>,
    pub url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl WebhookCapture {
    pub async fn start() -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
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

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_rx.await.ok();
                })
                .await
                .ok();
        });

        let url = format!("http://127.0.0.1:{}/hook", port);
        Self {
            received,
            url,
            _shutdown: shutdown_tx,
        }
    }
}
