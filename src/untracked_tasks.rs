use dashmap::DashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};
use tracing::{field, span, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Global map from spawn location (`file:line`) to current number of alive
/// untracked tokio tasks. Populated by [`UntrackedTaskLayer`].
static UNTRACKED_SPAWN_COUNTS: LazyLock<DashMap<String, AtomicI64>> =
    LazyLock::new(DashMap::new);

/// Returns a snapshot of currently-alive untracked task counts by spawn
/// location, sorted descending by count.
pub fn snapshot() -> Vec<(String, i64)> {
    let mut results: Vec<_> = UNTRACKED_SPAWN_COUNTS
        .iter()
        .filter(|e| e.value().load(Ordering::Relaxed) > 0)
        .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed)))
        .collect();
    results.sort_by(|a, b| b.1.cmp(&a.1));
    results
}

/// A tracing [`Layer`] that intercepts tokio's `runtime.spawn` spans and
/// records each spawn origin that does **not** go through our tracked
/// [`spawn`](crate::utils::spawn) / [`media_spawn`](crate::utils::media_spawn)
/// wrappers.
///
/// The counts are accessible via [`snapshot()`] and exposed in the AMI health
/// endpoint under `tokio.untracked`.
///
/// Install once at startup:
/// ```ignore
/// tracing_subscriber::registry()
///     .with(UntrackedTaskLayer::default())
///     .try_init()?;
/// ```
#[derive(Default, Clone)]
pub struct UntrackedTaskLayer;

/// Extension stored inside the `runtime.spawn` span so we can decrement the
/// counter when the task finishes (span is dropped).
struct UntrackedLoc(String);

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for UntrackedTaskLayer {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if attrs.metadata().target() != "tokio::task"
            || attrs.metadata().name() != "runtime.spawn"
        {
            return;
        }

        let mut loc = None;
        attrs.record(&mut LocVisitor(&mut loc));
        let loc = match loc {
            Some(l) => l,
            None => return,
        };

        // Skip spawns from our tracked wrappers (utils::spawn / utils::media_spawn)
        // which call tokio::spawn / handle.spawn from known lines inside utils.rs.
        if loc.starts_with("src/utils.rs:") {
            return;
        }

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(UntrackedLoc(loc.clone()));
            let counter = UNTRACKED_SPAWN_COUNTS
                .entry(loc)
                .or_insert_with(|| AtomicI64::new(0));
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            if let Some(loc) = span.extensions().get::<UntrackedLoc>() {
                if let Some(entry) = UNTRACKED_SPAWN_COUNTS.get(&loc.0) {
                    let prev = entry.fetch_sub(1, Ordering::Relaxed);
                    if prev <= 1 {
                        drop(entry);
                        UNTRACKED_SPAWN_COUNTS
                            .remove_if(&loc.0, |_, v| v.load(Ordering::Relaxed) <= 0);
                    }
                }
            }
        }
    }
}

struct LocVisitor<'a>(&'a mut Option<String>);

impl<'a> field::Visit for LocVisitor<'a> {
    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "loc" {
            let s = format!("{:?}", value);
            *self.0 = Some(s.trim_matches('"').to_string());
        }
    }
    fn record_str(&mut self, field: &field::Field, value: &str) {
        if field.name() == "loc" {
            *self.0 = Some(value.to_string());
        }
    }
}
