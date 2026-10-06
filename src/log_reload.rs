use std::sync::Arc;
use std::sync::OnceLock;
use tracing::Subscriber;
use tracing_subscriber::{EnvFilter, Layer};

type SharedFilter = Arc<parking_lot::Mutex<EnvFilter>>;

/// A [`Layer`] that delegates to an [`EnvFilter`] which can be swapped at
/// runtime via [`FilterHandle`]. Implements `Layer<S>` for *any* subscriber
/// `S`, so it can be placed anywhere in a layered subscriber stack.
pub struct ReloadableFilterLayer {
    inner: SharedFilter,
}

/// Handle for modifying the filter inside a [`ReloadableFilterLayer`] at
/// runtime without restarting the subscriber.
#[derive(Clone)]
pub struct FilterHandle {
    inner: SharedFilter,
}

impl ReloadableFilterLayer {
    pub fn new(filter: EnvFilter) -> (Self, FilterHandle) {
        let inner = Arc::new(parking_lot::Mutex::new(filter));
        (
            ReloadableFilterLayer {
                inner: inner.clone(),
            },
            FilterHandle { inner },
        )
    }
}

impl FilterHandle {
    pub fn modify(&self, f: impl FnOnce(&mut EnvFilter)) {
        let mut guard = self.inner.lock();
        f(&mut *guard);
        // Cache rebuilding re-enters this layer; release the filter lock first.
        drop(guard);
        tracing::callsite::rebuild_interest_cache();
    }
}

impl<S> Layer<S> for ReloadableFilterLayer
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        self.inner.lock().enabled(metadata, ctx)
    }

    fn event_enabled(
        &self,
        event: &tracing::Event<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        self.inner.lock().enabled(event.metadata(), ctx)
    }
}

static LOG_FILTER_HANDLE: OnceLock<FilterHandle> = OnceLock::new();

pub fn set_log_filter_handle(handle: FilterHandle) {
    let _ = LOG_FILTER_HANDLE.set(handle);
}

/// Apply a new log level at runtime without restarting the service.
/// Preserves the same noisy-crate suppression as startup (`hyper_util=warn`,
/// `rustls=warn`, `sqlx=warn`) and keeps rustrtc at `info` (silences its
/// DEBUG lifecycle noise while keeping the INFO media milestones).
pub fn apply_log_level(level: &str) -> Result<(), String> {
    let mut filter: EnvFilter = level
        .parse()
        .map_err(|e| format!("Invalid log level: {e}"))?;
    for noisy in &["hyper_util", "rustls", "sqlx"] {
        if let Ok(d) = format!("{}=warn", noisy).parse() {
            filter = filter.add_directive(d);
        }
    }
    if let Ok(d) = "rustrtc=info".parse() {
        filter = filter.add_directive(d);
    }
    let handle = LOG_FILTER_HANDLE
        .get()
        .ok_or_else(|| "log filter handle not initialized".to_string())?;
    handle.modify(|f| *f = filter);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use tracing_subscriber::prelude::*;

    #[derive(Clone)]
    struct LogBuffer(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn media_diagnostic() {
        tracing::debug!(target: "rustrtc::peer_connection", "media handshake diagnostic");
    }

    #[test]
    fn log_level_reload_enables_and_disables_an_existing_diagnostic_callsite() {
        let output = LogBuffer(Arc::new(parking_lot::Mutex::new(Vec::new())));
        let writer = output.clone();
        let (filter, handle) = ReloadableFilterLayer::new(EnvFilter::new("info,rustrtc=info"));
        set_log_filter_handle(handle);
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_ansi(false).without_time()
                .with_writer(move || writer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            media_diagnostic();
            assert!(output.0.lock().is_empty());
            apply_log_level("info,rustrtc::peer_connection=debug").unwrap();
            media_diagnostic();
            let enabled = String::from_utf8(output.0.lock().clone()).unwrap();
            assert!(enabled.contains("media handshake diagnostic"), "{enabled}");
            apply_log_level("info").unwrap();
            media_diagnostic();
            assert_eq!(String::from_utf8(output.0.lock().clone()).unwrap(), enabled);
        });
    }
}
