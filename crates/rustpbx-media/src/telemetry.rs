//! Process-wide media telemetry aggregated from all active bridges.
//!
//! Every bridge's 5s stats task publishes receive/send deltas here
//! ([`MediaTelemetry::record_rx`] / [`MediaTelemetry::record_tx`]); the host's
//! local stats logger (Prometheus-independent) or a Prometheus exporter snapshots
//! the cumulative totals via [`MediaTelemetry::snapshot`] and computes rates /
//! loss%. Live bridge count is maintained by register/unregister.
//!
//! Buckets follow send/receive directions at the system level:
//! - `rx`: what this process received from peers (transport RX), loss estimated
//!   from the remote Sender Report, internal drops = received but not forwarded.
//! - `tx`: what this process sent to peers (egress), loss from RTCP RR fraction
//!   lost reported by receivers, internal drops = dropped before egress.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Snapshot of one direction's cumulative counters.
#[derive(Debug, Clone, Default)]
pub struct DirectionSnapshot {
    pub packets_total: u64,
    pub lost_total: u64,
    pub internal_drops_total: u64,
}

/// Snapshot of the whole media telemetry for the current tick.
#[derive(Debug, Clone, Default)]
pub struct MediaTelemetrySnapshot {
    pub active_bridges: usize,
    pub rx: DirectionSnapshot,
    pub tx: DirectionSnapshot,
}

#[derive(Debug)]
struct DirectionAccum {
    packets_total: AtomicU64,
    lost_total: AtomicU64,
    internal_drops_total: AtomicU64,
}

impl DirectionAccum {
    fn new() -> Self {
        Self {
            packets_total: AtomicU64::new(0),
            lost_total: AtomicU64::new(0),
            internal_drops_total: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> DirectionSnapshot {
        DirectionSnapshot {
            packets_total: self.packets_total.load(Ordering::Relaxed),
            lost_total: self.lost_total.load(Ordering::Relaxed),
            internal_drops_total: self.internal_drops_total.load(Ordering::Relaxed),
        }
    }
}

pub struct MediaTelemetry {
    active_bridges: AtomicUsize,
    rx: DirectionAccum,
    tx: DirectionAccum,
}

impl MediaTelemetry {
    fn new() -> Self {
        Self {
            active_bridges: AtomicUsize::new(0),
            rx: DirectionAccum::new(),
            tx: DirectionAccum::new(),
        }
    }
}

fn global() -> &'static MediaTelemetry {
    static GLOBAL: OnceLock<MediaTelemetry> = OnceLock::new();
    GLOBAL.get_or_init(MediaTelemetry::new)
}

impl MediaTelemetry {
    /// Track a live bridge. Must be balanced with [`Self::unregister_bridge`].
    pub fn register_bridge() {
        let g = global();
        g.active_bridges.fetch_add(1, Ordering::Relaxed);
    }

    /// Remove a live bridge (call at teardown / Drop).
    pub fn unregister_bridge() {
        let g = global();
        g.active_bridges.fetch_sub(1, Ordering::Relaxed);
    }

    /// Publish a receive-direction delta (a single 5s window).
    pub fn record_rx(packets_d: u64, lost_d: u64, internal_drops_d: u64) {
        let rx = &global().rx;
        rx.packets_total.fetch_add(packets_d, Ordering::Relaxed);
        rx.lost_total.fetch_add(lost_d, Ordering::Relaxed);
        rx.internal_drops_total
            .fetch_add(internal_drops_d, Ordering::Relaxed);
    }

    /// Publish a send-direction delta (a single 5s window).
    pub fn record_tx(packets_d: u64, lost_d: u64, internal_drops_d: u64) {
        let tx = &global().tx;
        tx.packets_total.fetch_add(packets_d, Ordering::Relaxed);
        tx.lost_total.fetch_add(lost_d, Ordering::Relaxed);
        tx.internal_drops_total
            .fetch_add(internal_drops_d, Ordering::Relaxed);
    }

    /// Cumulative snapshot. Returns all-zero defaults when no bridge has ever
    /// published (safe to call from the host before any media is active).
    pub fn snapshot() -> MediaTelemetrySnapshot {
        let g = global();
        MediaTelemetrySnapshot {
            active_bridges: g.active_bridges.load(Ordering::Relaxed),
            rx: g.rx.snapshot(),
            tx: g.tx.snapshot(),
        }
    }

    /// Reset all counters and bridge count (used by tests; no-op otherwise).
    pub fn reset() {
        let g = global();
        g.active_bridges.store(0, Ordering::Relaxed);
        for a in [
            &g.rx.packets_total,
            &g.rx.lost_total,
            &g.rx.internal_drops_total,
            &g.tx.packets_total,
            &g.tx.lost_total,
            &g.tx.internal_drops_total,
        ] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_and_snapshots() {
        MediaTelemetry::reset();
        MediaTelemetry::register_bridge();
        MediaTelemetry::register_bridge();
        MediaTelemetry::record_rx(1000, 10, 2);
        MediaTelemetry::record_tx(950, 5, 0);

        let snap = MediaTelemetry::snapshot();
        assert_eq!(snap.active_bridges, 2);
        assert_eq!(snap.rx.packets_total, 1000);
        assert_eq!(snap.rx.lost_total, 10);
        assert_eq!(snap.rx.internal_drops_total, 2);
        assert_eq!(snap.tx.packets_total, 950);
        assert_eq!(snap.tx.lost_total, 5);
        MediaTelemetry::reset();
    }

    #[test]
    fn empty_snapshot_is_safe() {
        MediaTelemetry::reset();
        let snap = MediaTelemetry::snapshot();
        assert_eq!(snap.active_bridges, 0);
        assert_eq!(snap.rx.packets_total, 0);
    }
}
