//! Periodic Prometheus sampler for process-level and media-aggregate metrics.
//!
//! Two kinds of values cannot be incremented at the exact moment something
//! happens and are sampled on a fixed interval instead:
//!
//! * Kernel counters (`/proc`) — CPU time, RSS, open fds, TCP connections →
//!   `rustpbx_process_*` / `rustpbx_network_connections`. The readers are
//!   Linux-only and degrade to no-ops elsewhere, so spawning is safe on any
//!   platform.
//! * The process-wide [`MediaTelemetry`] aggregate — cumulative RTP packet /
//!   loss totals are converted to deltas and fed into the `rustpbx_rtp_*`
//!   counters, and the active-bridge gauge is refreshed.
//!
//! Also seeds the static gauges (`rustpbx_info`, uptime) on start.

use crate::media::telemetry::{MediaTelemetry, MediaTelemetrySnapshot};
use tokio_util::sync::CancellationToken;

const SAMPLE_INTERVAL_SECS: u64 = 15;

/// Spawn the sampler task; terminate when `cancel` fires.
pub fn spawn(cancel: CancellationToken) {
    crate::metrics::init_static_gauges();
    crate::utils::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(
            SAMPLE_INTERVAL_SECS,
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut prev_cpu_secs: Option<u64> = None;
        let mut prev_media = MediaTelemetrySnapshot::default();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {
                    sample_system(&mut prev_cpu_secs);
                    sample_media(&mut prev_media);
                }
            }
        }
    });
}

/// One sampling pass over the kernel counters. `pub(crate)` so the
/// observability addon can integration-test a pass against its recorder
/// (core must not depend on addon code, even in tests).
pub(crate) fn sample_system(prev_cpu_secs: &mut Option<u64>) {
    if let Some(total) = crate::proc_stats::process_cpu_seconds() {
        let delta = prev_cpu_secs.map(|p| total.saturating_sub(p)).unwrap_or(0);
        *prev_cpu_secs = Some(total);
        if delta > 0 {
            crate::metrics::system::process_cpu_seconds(delta);
        }
    }
    if let Some(bytes) = crate::proc_stats::resident_memory_bytes() {
        crate::metrics::system::set_process_memory_bytes(bytes);
    }
    if let Some(fds) = crate::proc_stats::open_fds() {
        crate::metrics::system::set_open_fds(fds);
    }
    if let Some(conns) = crate::proc_stats::network_connections() {
        crate::metrics::system::set_network_connections(conns);
    }
    crate::metrics::system::set_uptime_seconds();
}

/// One sampling pass over the media telemetry aggregate.
pub(crate) fn sample_media(prev: &mut MediaTelemetrySnapshot) {
    let cur = MediaTelemetry::snapshot();
    let rx_packets = cur.rx.packets_total.saturating_sub(prev.rx.packets_total);
    let tx_packets = cur.tx.packets_total.saturating_sub(prev.tx.packets_total);
    let rx_lost = cur.rx.lost_total.saturating_sub(prev.rx.lost_total);
    let tx_lost = cur.tx.lost_total.saturating_sub(prev.tx.lost_total);
    if rx_packets > 0 {
        crate::metrics::media::rtp_packets_received(rx_packets);
    }
    if tx_packets > 0 {
        crate::metrics::media::rtp_packets_sent(tx_packets);
    }
    if rx_lost > 0 {
        crate::metrics::media::rtp_packets_lost(rx_lost, "rx");
    }
    if tx_lost > 0 {
        crate::metrics::media::rtp_packets_lost(tx_lost, "tx");
    }
    crate::metrics::media::set_active_bridges(cur.active_bridges);
    *prev = cur;
}
