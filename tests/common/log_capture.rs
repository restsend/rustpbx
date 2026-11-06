//! Capture formatted diagnostic output without installing a global subscriber.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    pub(crate) fn start() -> (Self, tracing::subscriber::DefaultGuard, tracing::Dispatch) {
        Self::start_filtered("trace")
    }

    pub(crate) fn start_filtered(
        filter: &str,
    ) -> (Self, tracing::subscriber::DefaultGuard, tracing::Dispatch) {
        Self::start_filtered_with_timer(filter, ())
    }

    fn start_filtered_with_timer<
        T: tracing_subscriber::fmt::time::FormatTime + Send + Sync + 'static,
    >(
        filter: &str,
        timer: T,
    ) -> (Self, tracing::subscriber::DefaultGuard, tracing::Dispatch) {
        // Keep callsite interest dynamic when parallel tests use other dispatches.
        let interest = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new());
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_thread_names(true)
            .with_timer(timer)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber), interest)
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    pub(crate) async fn search(&self, call_id: &str) -> Vec<String> {
        let file = tempfile::NamedTempFile::new().unwrap();
        let contents = self
            .text()
            .lines()
            .map(|line| format!("2026-10-04T10:00:01Z {line}\n"))
            .collect::<String>();
        std::fs::write(file.path(), contents).unwrap();
        let query = crate::log_viewer::SearchQuery {
            node: Some("local".into()),
            start: "2026-10-04T10:00:00Z".parse().unwrap(),
            end: "2026-10-04T10:01:00Z".parse().unwrap(),
            call_ids: vec![call_id.into()],
            cursor: None,
            limit: Some(200),
        };
        for attempt in 0..60 {
            let response = crate::log_viewer::search_response(
                Some(file.path().to_string_lossy().into_owned()),
                query.clone(),
            )
            .await;
            if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS && attempt < 59 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["complete"], true);
            assert_eq!(body["gaps"], serde_json::json!([]));
            return body["lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| line.as_str().unwrap().to_owned())
                .collect();
        }
        unreachable!("search retry loop always returns");
    }
}
