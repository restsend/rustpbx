use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Shared performance counters for sipflow (atomics, no contention on hot path).
pub struct PerfCounters {
    pub packets_received: AtomicU64,
    pub items_recorded: AtomicU64,
    pub items_dropped: AtomicU64,
    pub flushes: AtomicU64,
    /// Explicitly-set pending/backlog count (e.g. items in channel or batch).
    /// Set via [`Self::set_pending`] from wherever the backlog is observable.
    pub pending: AtomicI64,
    /// Number of signaling (SIP) items successfully enqueued to the remote
    /// backend's ingest channel. Incremented in `RemoteBackend::record` on
    /// the hot path.
    pub signaling_sent: AtomicU64,
    /// Number of media (RTP) items successfully enqueued to the remote
    /// backend's ingest channel.
    pub media_sent: AtomicU64,
}

impl PerfCounters {
    pub fn new_arc() -> Arc<Self> {
        Arc::new(Self {
            packets_received: AtomicU64::new(0),
            items_recorded: AtomicU64::new(0),
            items_dropped: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            pending: AtomicI64::new(0),
            signaling_sent: AtomicU64::new(0),
            media_sent: AtomicU64::new(0),
        })
    }

    pub fn set_pending(&self, n: i64) {
        self.pending.store(n, Ordering::Relaxed);
    }
}

/// Default dump interval (10 s) — used by [`PerfDumper::new`] to preserve
/// the historical behavior of the local backend / standalone binary.
const DEFAULT_DUMP_INTERVAL: Duration = Duration::from_secs(10);

/// Perf-dumper that tracks snapshot state.  Call `try_dump()` periodically;
/// it returns a log message at most once every `dump_interval`, and exports
/// counters to `metrics` on every dump.
pub struct PerfDumper {
    counters: Arc<PerfCounters>,
    dump_interval: Duration,
    last_time: Instant,
    last_received: u64,
    last_recorded: u64,
    last_dropped: u64,
    last_flushes: u64,
    last_signaling_sent: u64,
    last_media_sent: u64,
}

impl PerfDumper {
    pub fn new(counters: Arc<PerfCounters>) -> Self {
        Self::with_interval(counters, DEFAULT_DUMP_INTERVAL)
    }

    /// Create a dumper with a custom dump interval (e.g. 1 second for the
    /// remote backend, which needs near-real-time visibility into drops).
    pub fn with_interval(counters: Arc<PerfCounters>, dump_interval: Duration) -> Self {
        Self {
            counters,
            dump_interval,
            last_time: Instant::now(),
            last_received: 0,
            last_recorded: 0,
            last_dropped: 0,
            last_flushes: 0,
            last_signaling_sent: 0,
            last_media_sent: 0,
        }
    }

    /// Returns `Some(msg)` if at least `dump_interval` has elapsed since the
    /// last dump, `None` otherwise. Always exports counters to `metrics`
    /// when it returns `Some`.
    pub fn try_dump(&mut self) -> Option<String> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_time).as_secs_f64();
        if elapsed < self.dump_interval.as_secs_f64() {
            return None;
        }

        let received = self.counters.packets_received.load(Ordering::Relaxed);
        let recorded = self.counters.items_recorded.load(Ordering::Relaxed);
        let dropped = self.counters.items_dropped.load(Ordering::Relaxed);
        let flushes = self.counters.flushes.load(Ordering::Relaxed);
        let pending = self.counters.pending.load(Ordering::Relaxed);
        let signaling_sent = self.counters.signaling_sent.load(Ordering::Relaxed);
        let media_sent = self.counters.media_sent.load(Ordering::Relaxed);

        let dr = received - self.last_received;
        let dw = recorded - self.last_recorded;
        let dd = dropped - self.last_dropped;
        let df = flushes - self.last_flushes;
        let d_sig = signaling_sent - self.last_signaling_sent;
        let d_media = media_sent - self.last_media_sent;

        self.last_received = received;
        self.last_recorded = recorded;
        self.last_dropped = dropped;
        self.last_flushes = flushes;
        self.last_signaling_sent = signaling_sent;
        self.last_media_sent = media_sent;
        self.last_time = now;

        // ── export to metrics ──
        metrics::counter!("sipflow_packets_received_total", "component" => "sipflow").increment(dr);
        metrics::counter!("sipflow_items_recorded_total", "component" => "sipflow").increment(dw);
        metrics::counter!("sipflow_items_dropped_total", "component" => "sipflow").increment(dd);
        metrics::counter!("sipflow_flushes_total", "component" => "sipflow").increment(df);
        metrics::counter!("sipflow_signaling_sent_total", "component" => "sipflow").increment(d_sig);
        metrics::counter!("sipflow_media_sent_total", "component" => "sipflow").increment(d_media);
        metrics::gauge!("sipflow_pending_items", "component" => "sipflow").set(pending as f64);

        Some(format!(
            "sipflow perf: sig_sent={:.1}/s  media_sent={:.1}/s  drop={:.1}/s  recv={:.1}/s  record={:.1}/s  flush={:.1}/s  pending={}  total_sig={}  total_media={}  total_drop={}",
            d_sig as f64 / elapsed,
            d_media as f64 / elapsed,
            dd as f64 / elapsed,
            dr as f64 / elapsed,
            dw as f64 / elapsed,
            df as f64 / elapsed,
            pending,
            signaling_sent,
            media_sent,
            dropped,
        ))
    }
}
