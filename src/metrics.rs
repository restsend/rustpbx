//! Process metrics helpers and console UI types for the scrape endpoint.

/// Console metrics page hints for the Prometheus scrape endpoint.
///
/// Populated by the observability addon via
/// [`crate::addons::Addon::metrics_endpoint_info`] — core never loads addon config.
#[derive(Debug, Clone)]
pub struct MetricsEndpointInfo {
    pub enabled: bool,
    pub path: String,
    pub auth_required: bool,
}

pub mod sip {
    pub fn registration_received(realm: &str) {
        metrics::counter!(
            "rustpbx_sip_registrations_total",
            "realm" => realm.to_string()
        )
        .increment(1);
    }

    pub fn registration_succeeded(realm: &str) {
        metrics::counter!(
            "rustpbx_sip_registrations_succeeded_total",
            "realm" => realm.to_string()
        )
        .increment(1);
    }

    pub fn registration_failed(realm: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_sip_registrations_failed_total",
            "realm" => realm.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn unregistration(realm: &str) {
        metrics::counter!(
            "rustpbx_sip_unregistrations_total",
            "realm" => realm.to_string()
        )
        .increment(1);
    }

    pub fn set_active_registrations(count: usize) {
        metrics::gauge!("rustpbx_sip_registrations_active").set(count as f64);
    }

    pub fn dialog_created(direction: &str) {
        metrics::counter!(
            "rustpbx_sip_dialogs_created_total",
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    pub fn dialog_terminated(direction: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_sip_dialogs_terminated_total",
            "direction" => direction.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn set_active_dialogs(count: usize) {
        metrics::gauge!("rustpbx_sip_dialogs_active").set(count as f64);
    }

    /// 1 while the node is draining (graceful shutdown in progress):
    /// new calls/registrations rejected, waiting for active calls to
    /// end before exit. Sampled alongside the dialog gauge.
    pub fn set_draining(draining: bool) {
        metrics::gauge!("rustpbx_draining").set(draining as u8 as f64);
    }

    /// WebRTC (WS) transport locations currently registered, sampled from the
    /// registration locator.
    pub fn set_webrtc_locations(count: usize) {
        metrics::gauge!("rustpbx_sip_webrtc_locations_active").set(count as f64);
    }

    pub fn invite_latency_seconds(duration_secs: f64, direction: &str) {
        metrics::histogram!(
            "rustpbx_sip_invite_latency_seconds",
            "direction" => direction.to_string()
        )
        .record(duration_secs);
    }
}

/// Session event-loop heartbeat gauges (sampled from the active-call
/// registry every 5s; `stale` uses the same 120s threshold as the CC
/// stuck-binding watchdog).
pub mod session {
    pub fn set_heartbeat_tracked(count: usize) {
        metrics::gauge!("rustpbx_session_heartbeats_tracked").set(count as f64);
    }
    pub fn set_heartbeat_max_age(secs: u64) {
        metrics::gauge!("rustpbx_session_heartbeat_max_age_secs").set(secs as f64);
    }
    pub fn set_heartbeat_stale(count: usize) {
        metrics::gauge!("rustpbx_session_heartbeats_stale").set(count as f64);
    }
}

pub mod transaction {
    pub fn received() {
        metrics::counter!("rustpbx_sip_transactions_received_total").increment(1);
    }

    pub fn rejected(reason: &str) {
        metrics::counter!(
            "rustpbx_sip_transactions_rejected_total",
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn set_running(count: usize) {
        metrics::gauge!("rustpbx_sip_transactions_running").set(count as f64);
    }

    pub fn latency_seconds(duration_secs: f64) {
        metrics::histogram!("rustpbx_sip_transaction_latency_seconds").record(duration_secs);
    }

    pub fn set_endpoint_running(count: usize) {
        metrics::gauge!("rustpbx_sip_endpoint_running_transactions").set(count as f64);
    }

    pub fn set_endpoint_finished(count: usize) {
        metrics::gauge!("rustpbx_sip_endpoint_finished_transactions").set(count as f64);
    }

    pub fn set_endpoint_waiting_ack(count: usize) {
        metrics::gauge!("rustpbx_sip_endpoint_waiting_ack").set(count as f64);
    }
}

pub mod media {
    /// Cumulative RTP packets sent, fed by the periodic sampler from the
    /// media telemetry aggregate (delta per tick).
    pub fn rtp_packets_sent(count: u64) {
        metrics::counter!("rustpbx_rtp_packets_sent_total").increment(count);
    }

    /// Cumulative RTP packets received, fed by the periodic sampler from the
    /// media telemetry aggregate (delta per tick).
    pub fn rtp_packets_received(count: u64) {
        metrics::counter!("rustpbx_rtp_packets_received_total").increment(count);
    }

    /// Cumulative RTP packets lost, fed by the periodic sampler from the
    /// media telemetry aggregate (delta per tick). `direction` is `rx`
    /// (we did not receive) or `tx` (remote reported loss via RTCP).
    pub fn rtp_packets_lost(count: u64, direction: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_lost_total",
            "direction" => direction.to_string()
        )
        .increment(count);
    }

    /// Live media bridges (conference / recording sessions), sampled from the
    /// media telemetry aggregate.
    pub fn set_active_bridges(count: usize) {
        metrics::gauge!("rustpbx_media_active_bridges").set(count as f64);
    }
}

pub mod db {
    pub fn query_latency_seconds(op: &str, duration_secs: f64) {
        metrics::histogram!(
            "rustpbx_db_query_latency_seconds",
            "op" => op.to_string()
        )
        .record(duration_secs);
    }

    pub fn slow_query_total(op: &str, threshold_ms: u64) {
        metrics::counter!(
            "rustpbx_db_slow_query_total",
            "op" => op.to_string(),
            "threshold_ms" => threshold_ms.to_string()
        )
        .increment(1);
    }
}

pub mod system {
    use std::sync::OnceLock;

    static START_TIME: OnceLock<std::time::Instant> = OnceLock::new();

    fn get_start_time() -> std::time::Instant {
        *START_TIME.get_or_init(std::time::Instant::now)
    }

    pub fn process_cpu_seconds(total_secs: u64) {
        metrics::counter!("rustpbx_process_cpu_seconds_total").increment(total_secs);
    }

    pub fn set_process_memory_bytes(bytes: u64) {
        metrics::gauge!("rustpbx_process_resident_memory_bytes").set(bytes as f64);
    }

    pub fn set_open_fds(count: usize) {
        metrics::gauge!("rustpbx_process_open_fds").set(count as f64);
    }

    pub fn set_network_connections(count: usize) {
        metrics::gauge!("rustpbx_network_connections").set(count as f64);
    }

    pub fn set_uptime_seconds() {
        let uptime = get_start_time().elapsed().as_secs() as f64;
        metrics::gauge!("rustpbx_process_uptime_seconds").set(uptime);
    }
}

pub mod transfer {
    pub fn attempt_total(mode: &str, direction: &str) {
        metrics::counter!(
            "rustpbx_transfer_attempt_total",
            "mode" => mode.to_string(),
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    pub fn success_total(mode: &str) {
        metrics::counter!(
            "rustpbx_transfer_success_total",
            "mode" => mode.to_string()
        )
        .increment(1);
    }

    pub fn failed_total(mode: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_transfer_failed_total",
            "mode" => mode.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn set_active_transfers(count: usize) {
        metrics::gauge!("rustpbx_transfer_active").set(count as f64);
    }
}

pub mod conference {
    pub fn created() {
        metrics::counter!("rustpbx_conference_created_total").increment(1);
    }

    /// Emitted when a media bridge handle is dropped (the bridge terminates
    /// with the session that owns it).
    pub fn destroyed(reason: &str) {
        metrics::counter!(
            "rustpbx_conference_destroyed_total",
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    /// Aggregate bytes mixed into conference outputs. Intentionally has no
    /// per-conference label — conference IDs are unbounded and Prometheus
    /// series never expire.
    pub fn media_injected_bytes(bytes: u64) {
        metrics::counter!("rustpbx_conference_media_injected_bytes_total").increment(bytes);
    }
}


pub mod cdr {
    /// Record accepted into the bounded queue (producer → manager).
    pub fn enqueued() {
        metrics::counter!("cdr_records_enqueued_total").increment(1);
    }

    /// Record lost because the queue was full or the manager was gone.
    pub fn dropped() {
        metrics::counter!("cdr_records_dropped_total").increment(1);
    }

    /// Records handed to the saver and persisted successfully.
    pub fn pushed(n: u64) {
        metrics::counter!("cdr_records_pushed_total").increment(n);
    }

    /// Records in a batch the saver failed to persist.
    pub fn push_failed(n: u64) {
        metrics::counter!("cdr_records_push_failed_total").increment(n);
    }

    pub fn set_queue_size(capacity: usize) {
        metrics::gauge!("cdr_queue_size").set(capacity as f64);
    }

    pub fn set_queue_current(n: usize) {
        metrics::gauge!("cdr_queue_current").set(n as f64);
    }

    /// Queueing wait (record enqueued → manager dequeued). Excludes the
    /// save/push time itself; a slow endpoint does NOT inflate this —
    /// queue backlog does. Opt-in via `[callrecord] track_queue_latency`.
    pub fn queue_latency_seconds(duration_secs: f64) {
        metrics::histogram!("cdr_queue_latency_seconds").record(duration_secs);
    }
}

pub mod recording {
    /// Hangup/first-fail → successful upload latency.
    pub fn upload_latency_seconds(duration_secs: f64, destination: &str) {
        metrics::histogram!(
            "rustpbx_recording_upload_latency_seconds",
            "destination" => destination.to_string()
        )
        .record(duration_secs);
    }

    pub fn upload_success(destination: &str) {
        metrics::counter!(
            "rustpbx_recording_upload_success_total",
            "destination" => destination.to_string()
        )
        .increment(1);
    }

    pub fn upload_failure(destination: &str) {
        metrics::counter!(
            "rustpbx_recording_upload_failure_total",
            "destination" => destination.to_string()
        )
        .increment(1);
    }

    /// Upload completed after the configured SLA window (default 10 min).
    pub fn upload_sla_breach(destination: &str) {
        metrics::counter!(
            "rustpbx_recording_upload_sla_breach_total",
            "destination" => destination.to_string()
        )
        .increment(1);
    }

    pub fn retry_attempt(destination: &str) {
        metrics::counter!(
            "rustpbx_recording_upload_retry_total",
            "destination" => destination.to_string()
        )
        .increment(1);
    }

    pub fn set_pending_failed(count: usize) {
        metrics::gauge!("rustpbx_recording_upload_pending_failed").set(count as f64);
    }
}

pub fn init_static_gauges() {
    let version = crate::version::get_short_version();
    metrics::gauge!("rustpbx_info", "version" => version).set(1.0);

    system::set_uptime_seconds();
}
