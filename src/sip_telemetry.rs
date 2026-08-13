//! Local, Prometheus-independent SIP counters consumed by the local stats log.
//!
//! The Prometheus recorder already captures transaction volume and latency via
//! `crate::metrics`, but the local stats log must work without any recorder
//! installed. These atomics mirror the per-transaction measurements taken in
//! `proxy/server.rs` (incoming transaction count + transaction execution time)
//! so the stats logger can report message volume and tx latency as deltas.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Cumulative snapshot of the local SIP counters.
#[derive(Debug, Clone, Default)]
pub struct SipTelemetrySnapshot {
    /// Total incoming SIP transactions processed.
    pub tx_received_total: u64,
    /// Sum of transaction execution times in microseconds.
    pub tx_latency_sum_us: u64,
    /// Number of transactions with a recorded latency.
    pub tx_latency_count: u64,
    /// Max transaction execution time in microseconds.
    pub tx_latency_max_us: u64,
}

#[derive(Debug)]
pub struct SipTelemetry {
    tx_received_total: AtomicU64,
    tx_latency_sum_us: AtomicU64,
    tx_latency_count: AtomicU64,
    tx_latency_max_us: AtomicU64,
}

impl SipTelemetry {
    fn new() -> Self {
        Self {
            tx_received_total: AtomicU64::new(0),
            tx_latency_sum_us: AtomicU64::new(0),
            tx_latency_count: AtomicU64::new(0),
            tx_latency_max_us: AtomicU64::new(0),
        }
    }

    fn global() -> &'static SipTelemetry {
        static GLOBAL: OnceLock<SipTelemetry> = OnceLock::new();
        GLOBAL.get_or_init(SipTelemetry::new)
    }

    /// An incoming SIP transaction was accepted for processing.
    pub fn tx_received() {
        SipTelemetry::global()
            .tx_received_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a transaction's execution time.
    pub fn record_tx_latency(elapsed: Duration) {
        let g = SipTelemetry::global();
        let us = elapsed.as_micros() as u64;
        g.tx_latency_sum_us.fetch_add(us, Ordering::Relaxed);
        g.tx_latency_count.fetch_add(1, Ordering::Relaxed);
        g.tx_latency_max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Cumulative snapshot.
    pub fn snapshot() -> SipTelemetrySnapshot {
        let g = SipTelemetry::global();
        SipTelemetrySnapshot {
            tx_received_total: g.tx_received_total.load(Ordering::Relaxed),
            tx_latency_sum_us: g.tx_latency_sum_us.load(Ordering::Relaxed),
            tx_latency_count: g.tx_latency_count.load(Ordering::Relaxed),
            tx_latency_max_us: g.tx_latency_max_us.load(Ordering::Relaxed),
        }
    }

    /// Reset all counters (tests).
    pub fn reset() {
        let g = SipTelemetry::global();
        g.tx_received_total.store(0, Ordering::Relaxed);
        g.tx_latency_sum_us.store(0, Ordering::Relaxed);
        g.tx_latency_count.store(0, Ordering::Relaxed);
        g.tx_latency_max_us.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_and_snapshots() {
        SipTelemetry::reset();
        SipTelemetry::tx_received();
        SipTelemetry::record_tx_latency(Duration::from_millis(12));
        SipTelemetry::record_tx_latency(Duration::from_millis(30));
        SipTelemetry::tx_received();

        let snap = SipTelemetry::snapshot();
        assert_eq!(snap.tx_received_total, 2);
        assert_eq!(snap.tx_latency_count, 2);
        assert_eq!(snap.tx_latency_sum_us, 42_000);
        assert_eq!(snap.tx_latency_max_us, 30_000);
        SipTelemetry::reset();
    }
}
