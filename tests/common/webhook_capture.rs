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
        Self::start_with_response(&["/hook"], serde_json::json!({"status":"ok"})).await
    }

    pub async fn start_with_response(paths: &[&str], response: serde_json::Value) -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let mut app = axum::Router::new();
        for path in paths {
            let rc = received.clone();
            let response = response.clone();
            app = app.route(path, axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                rc.lock().unwrap().push(body);
                let response = response.clone();
                async { axum::Json(response) }
            }));
        }

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
