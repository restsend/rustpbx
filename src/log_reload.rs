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
/// `rustls=warn`, `sqlx=warn`).
pub fn apply_log_level(level: &str) -> Result<(), String> {
    let mut filter: EnvFilter = level
        .parse()
        .map_err(|e| format!("Invalid log level: {e}"))?;
    for noisy in &["hyper_util", "rustls", "sqlx"] {
        if let Ok(d) = format!("{}=warn", noisy).parse() {
            filter = filter.add_directive(d);
        }
    }
    let handle = LOG_FILTER_HANDLE
        .get()
        .ok_or_else(|| "log filter handle not initialized".to_string())?;
    handle.modify(|f| *f = filter);
    Ok(())
}
