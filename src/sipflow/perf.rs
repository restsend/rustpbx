use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

/// Shared performance counters for sipflow (atomics, no contention on hot path).
pub struct PerfCounters {
    pub packets_received: AtomicU64,
    pub items_recorded: AtomicU64,
    pub items_dropped: AtomicU64,
    pub flushes: AtomicU64,
    /// Explicitly-set pending/backlog count (e.g. items in channel or batch).
    /// Set via [`Self::set_pending`] from wherever the backlog is observable.
    pub pending: AtomicI64,
}

impl PerfCounters {
    pub fn new_arc() -> Arc<Self> {
        Arc::new(Self {
            packets_received: AtomicU64::new(0),
            items_recorded: AtomicU64::new(0),
            items_dropped: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            pending: AtomicI64::new(0),
        })
    }

    pub fn set_pending(&self, n: i64) {
        self.pending.store(n, Ordering::Relaxed);
    }
}

/// Perf-dumper that tracks snapshot state.  Call `try_dump()` periodically;
/// it returns a log message at most once every 10 seconds, skipping periods
/// with zero activity, and exports counters to `metrics`.
pub struct PerfDumper {
    counters: Arc<PerfCounters>,
    last_time: Instant,
    last_received: u64,
    last_recorded: u64,
    last_dropped: u64,
    last_flushes: u64,
}

impl PerfDumper {
    pub fn new(counters: Arc<PerfCounters>) -> Self {
        Self {
            counters,
            last_time: Instant::now(),
            last_received: 0,
            last_recorded: 0,
            last_dropped: 0,
            last_flushes: 0,
        }
    }

    /// Returns `Some(msg)` every ~10 s if any counter changed, `None` otherwise.
    pub fn try_dump(&mut self) -> Option<String> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_time).as_secs_f64();
        if elapsed < 10.0 {
            return None;
        }

        let received = self.counters.packets_received.load(Ordering::Relaxed);
        let recorded = self.counters.items_recorded.load(Ordering::Relaxed);
        let dropped = self.counters.items_dropped.load(Ordering::Relaxed);
        let flushes = self.counters.flushes.load(Ordering::Relaxed);
        let pending = self.counters.pending.load(Ordering::Relaxed);

        let dr = received - self.last_received;
        let dw = recorded - self.last_recorded;
        let dd = dropped - self.last_dropped;
        let df = flushes - self.last_flushes;

        self.last_received = received;
        self.last_recorded = recorded;
        self.last_dropped = dropped;
        self.last_flushes = flushes;
        self.last_time = now;

        // ── export to metrics ──
        metrics::counter!("sipflow_packets_received_total", "component" => "sipflow").increment(dr);
        metrics::counter!("sipflow_items_recorded_total", "component" => "sipflow").increment(dw);
        metrics::counter!("sipflow_items_dropped_total", "component" => "sipflow").increment(dd);
        metrics::counter!("sipflow_flushes_total", "component" => "sipflow").increment(df);
        metrics::gauge!("sipflow_pending_items", "component" => "sipflow").set(pending as f64);

        if dr == 0 && dw == 0 && dd == 0 && df == 0 {
            return None;
        }

        Some(format!(
            "sipflow perf: recv={:.1}/s  record={:.1}/s  drop={:.1}/s  flush={:.1}/s  pending={}  total_recorded={}",
            dr as f64 / elapsed,
            dw as f64 / elapsed,
            dd as f64 / elapsed,
            df as f64 / elapsed,
            pending,
            recorded,
        ))
    }
}
