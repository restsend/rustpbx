pub mod sip {
    /// Increment when a REGISTER request is received
    pub fn registration_received(realm: &str) {
        metrics::counter!(
            "rustpbx_sip_registrations_total",
            "realm" => realm.to_string()
        )
        .increment(1);
    }

    /// Increment when a registration succeeds
    pub fn registration_succeeded(realm: &str) {
        metrics::counter!(
            "rustpbx_sip_registrations_succeeded_total",
            "realm" => realm.to_string()
        )
        .increment(1);
    }

    /// Increment when a registration fails
    pub fn registration_failed(realm: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_sip_registrations_failed_total",
            "realm" => realm.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    /// Increment when a registration is removed (unregister)
    pub fn unregistration(realm: &str) {
        metrics::counter!(
            "rustpbx_sip_unregistrations_total",
            "realm" => realm.to_string()
        )
        .increment(1);
    }

    /// Set the current number of active registrations
    pub fn set_active_registrations(count: usize) {
        metrics::gauge!("rustpbx_sip_registrations_active").set(count as f64);
    }

    /// Increment when a SIP dialog is created
    pub fn dialog_created(direction: &str) {
        metrics::counter!(
            "rustpbx_sip_dialogs_created_total",
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    /// Increment when a SIP dialog is terminated
    pub fn dialog_terminated(direction: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_sip_dialogs_terminated_total",
            "direction" => direction.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    /// Set the current number of active SIP dialogs
    pub fn set_active_dialogs(count: usize) {
        metrics::gauge!("rustpbx_sip_dialogs_active").set(count as f64);
    }

    /// Record SIP response code
    pub fn response(status_code: u16, method: &str) {
        let code_class = status_code / 100;
        metrics::counter!(
            "rustpbx_sip_responses_total",
            "status_class" => format!("{}xx", code_class),
            "status_code" => status_code.to_string(),
            "method" => method.to_string()
        )
        .increment(1);
    }

    /// Record SIP INVITE latency
    pub fn invite_latency_seconds(duration_secs: f64, direction: &str) {
        metrics::histogram!(
            "rustpbx_sip_invite_latency_seconds",
            "direction" => direction.to_string()
        )
        .record(duration_secs);
    }
}

pub mod trunk {
    /// Increment when a call is routed through a trunk
    pub fn call_routed(trunk_id: &str, direction: &str) {
        metrics::counter!(
            "rustpbx_trunk_calls_total",
            "trunk_id" => trunk_id.to_string(),
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    /// Increment when a trunk call fails
    pub fn call_failed(trunk_id: &str, direction: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_trunk_calls_failed_total",
            "trunk_id" => trunk_id.to_string(),
            "direction" => direction.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    /// Record trunk latency
    pub fn latency_seconds(trunk_id: &str, duration_secs: f64) {
        metrics::histogram!(
            "rustpbx_trunk_latency_seconds",
            "trunk_id" => trunk_id.to_string()
        )
        .record(duration_secs);
    }

    /// Set trunk status (1 = online, 0 = offline)
    pub fn set_status(trunk_id: &str, online: bool) {
        metrics::gauge!(
            "rustpbx_trunk_status",
            "trunk_id" => trunk_id.to_string()
        )
        .set(if online { 1.0 } else { 0.0 });
    }
}

/// Media (RTP) related metrics
pub mod media {
    /// Record RTP packets sent
    pub fn rtp_packets_sent(count: u64, codec: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_sent_total",
            "codec" => codec.to_string()
        )
        .increment(count);
    }

    /// Record RTP packets received
    pub fn rtp_packets_received(count: u64, codec: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_received_total",
            "codec" => codec.to_string()
        )
        .increment(count);
    }

    /// Record RTP packets lost
    pub fn rtp_packets_lost(count: u64, direction: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_lost_total",
            "direction" => direction.to_string()
        )
        .increment(count);
    }

    /// Record RTP jitter
    pub fn rtp_jitter_seconds(jitter_secs: f64, direction: &str) {
        metrics::histogram!(
            "rustpbx_rtp_jitter_seconds",
            "direction" => direction.to_string()
        )
        .record(jitter_secs);
    }

    /// Set codec usage count
    pub fn set_codec_usage(codec: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_media_codec_usage",
            "codec" => codec.to_string()
        )
        .set(count as f64);
    }

    /// Record WebRTC ICE connection establishment time
    pub fn ice_connection_time_seconds(duration_secs: f64) {
        metrics::histogram!("rustpbx_webrtc_ice_connection_seconds").record(duration_secs);
    }

    /// Increment WebRTC connection count
    pub fn webrtc_connection_created() {
        metrics::counter!("rustpbx_webrtc_connections_total").increment(1);
    }

    /// Increment WebRTC connection failure count
    pub fn webrtc_connection_failed(reason: &str) {
        metrics::counter!(
            "rustpbx_webrtc_connections_failed_total",
            "reason" => reason.to_string()
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

    /// Record process CPU seconds (called periodically)
    pub fn process_cpu_seconds(total_secs: u64) {
        metrics::counter!("rustpbx_process_cpu_seconds_total").increment(total_secs);
    }

    /// Set process resident memory in bytes
    pub fn set_process_memory_bytes(bytes: u64) {
        metrics::gauge!("rustpbx_process_resident_memory_bytes").set(bytes as f64);
    }

    /// Set number of open file descriptors
    pub fn set_open_fds(count: usize) {
        metrics::gauge!("rustpbx_process_open_fds").set(count as f64);
    }

    /// Set number of active network connections
    pub fn set_network_connections(count: usize) {
        metrics::gauge!("rustpbx_network_connections").set(count as f64);
    }

    /// Set process uptime in seconds
    pub fn set_uptime_seconds() {
        let uptime = get_start_time().elapsed().as_secs() as f64;
        metrics::gauge!("rustpbx_process_uptime_seconds").set(uptime);
    }

    /// Increment when a WebSocket connection is established
    pub fn websocket_connection_created() {
        metrics::counter!("rustpbx_websocket_connections_total").increment(1);
    }

    /// Increment when a WebSocket connection is closed
    pub fn websocket_connection_closed() {
        metrics::counter!("rustpbx_websocket_disconnections_total").increment(1);
    }

    /// Set current WebSocket connection count
    pub fn set_websocket_connections(count: usize) {
        metrics::gauge!("rustpbx_websocket_connections_active").set(count as f64);
    }
}

/// Voicemail related metrics
pub mod voicemail {
    /// Increment when a voicemail is received
    pub fn message_received(mailbox: &str) {
        metrics::counter!(
            "rustpbx_voicemail_messages_total",
            "mailbox" => mailbox.to_string()
        )
        .increment(1);
    }

    /// Record voicemail duration
    pub fn message_duration_seconds(duration_secs: f64, mailbox: &str) {
        metrics::histogram!(
            "rustpbx_voicemail_duration_seconds",
            "mailbox" => mailbox.to_string()
        )
        .record(duration_secs);
    }

    /// Set number of voicemails in mailbox
    pub fn set_message_count(mailbox: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_voicemail_messages_stored",
            "mailbox" => mailbox.to_string()
        )
        .set(count as f64);
    }
}

/// Queue related metrics
pub mod queue {
    /// Record queue wait time
    pub fn wait_time_seconds(duration_secs: f64, queue_name: &str) {
        metrics::histogram!(
            "rustpbx_queue_wait_time_seconds",
            "queue" => queue_name.to_string()
        )
        .record(duration_secs);
    }

    /// Set current queue size
    pub fn set_size(queue_name: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_queue_size",
            "queue" => queue_name.to_string()
        )
        .set(count as f64);
    }

    /// Increment when a caller abandons the queue
    pub fn caller_abandoned(queue_name: &str) {
        metrics::counter!(
            "rustpbx_queue_abandoned_total",
            "queue" => queue_name.to_string()
        )
        .increment(1);
    }

    /// Increment when a caller is answered from queue
    pub fn caller_answered(queue_name: &str) {
        metrics::counter!(
            "rustpbx_queue_answered_total",
            "queue" => queue_name.to_string()
        )
        .increment(1);
    }
}

/// Transcription related metrics
pub mod transcription {
    /// Increment when a transcription is requested
    pub fn request_received(language: &str) {
        metrics::counter!(
            "rustpbx_transcription_requests_total",
            "language" => language.to_string()
        )
        .increment(1);
    }

    /// Increment when a transcription succeeds
    pub fn request_succeeded(language: &str) {
        metrics::counter!(
            "rustpbx_transcription_success_total",
            "language" => language.to_string()
        )
        .increment(1);
    }

    /// Increment when a transcription fails
    pub fn request_failed(language: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_transcription_failed_total",
            "language" => language.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    /// Record transcription latency
    pub fn latency_seconds(duration_secs: f64, language: &str) {
        metrics::histogram!(
            "rustpbx_transcription_latency_seconds",
            "language" => language.to_string()
        )
        .record(duration_secs);
    }

    /// Record audio duration that was transcribed
    pub fn audio_duration_seconds(duration_secs: f64, language: &str) {
        metrics::histogram!(
            "rustpbx_transcription_audio_seconds",
            "language" => language.to_string()
        )
        .record(duration_secs);
    }
}

pub mod routing {
    /// Increment when a route is evaluated
    pub fn route_evaluated(direction: &str, matched: bool) {
        metrics::counter!(
            "rustpbx_routing_evaluations_total",
            "direction" => direction.to_string(),
            "matched" => matched.to_string()
        )
        .increment(1);
    }

    /// Increment when default route is used
    pub fn default_route_used(direction: &str) {
        metrics::counter!(
            "rustpbx_routing_default_route_total",
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    /// Record routing evaluation latency
    pub fn evaluation_latency_seconds(duration_secs: f64) {
        metrics::histogram!("rustpbx_routing_evaluation_seconds").record(duration_secs);
    }
}

pub mod auth {
    /// Increment when authentication is attempted
    pub fn auth_attempt(method: &str) {
        metrics::counter!(
            "rustpbx_auth_attempts_total",
            "method" => method.to_string()
        )
        .increment(1);
    }

    /// Increment when authentication succeeds
    pub fn auth_success(method: &str) {
        metrics::counter!(
            "rustpbx_auth_success_total",
            "method" => method.to_string()
        )
        .increment(1);
    }

    /// Increment when authentication fails
    pub fn auth_failure(method: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_auth_failure_total",
            "method" => method.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }
}

pub fn init_static_gauges() {
    let version = crate::version::get_short_version();
    metrics::gauge!("rustpbx_info", "version" => version).set(1.0);

    // Initialize system metrics baseline
    system::set_uptime_seconds();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_macros_compile() {
        // Just verify the macros compile correctly
        // Note: These won't record anything without a recorder installed
        sip::registration_received("localhost");
        sip::registration_succeeded("localhost");
        sip::registration_failed("localhost", "auth_failed");
        sip::unregistration("localhost");
        sip::set_active_registrations(5);
        sip::dialog_created("inbound");
        sip::dialog_terminated("inbound", "bye");
        sip::set_active_dialogs(10);
        sip::response(200, "INVITE");
        sip::invite_latency_seconds(0.5, "inbound");

        trunk::call_routed("trunk-1", "outbound");
        trunk::call_failed("trunk-1", "outbound", "timeout");
        trunk::latency_seconds("trunk-1", 0.1);
        trunk::set_status("trunk-1", true);

        media::rtp_packets_sent(100, "opus");
        media::rtp_packets_received(100, "opus");
        media::rtp_packets_lost(5, "inbound");
        media::rtp_jitter_seconds(0.01, "inbound");
        media::set_codec_usage("opus", 5);
        media::ice_connection_time_seconds(0.5);
        media::webrtc_connection_created();
        media::webrtc_connection_failed("ice_timeout");

        system::websocket_connection_created();
        system::websocket_connection_closed();
        system::set_websocket_connections(3);

        voicemail::message_received("1001");
        voicemail::message_duration_seconds(30.0, "1001");
        voicemail::set_message_count("1001", 5);

        queue::wait_time_seconds(10.0, "support");
        queue::set_size("support", 3);
        queue::caller_abandoned("support");
        queue::caller_answered("support");

        transcription::request_received("zh");
        transcription::request_succeeded("zh");
        transcription::request_failed("zh", "timeout");
        transcription::latency_seconds(2.0, "zh");
        transcription::audio_duration_seconds(30.0, "zh");

        routing::route_evaluated("outbound", true);
        routing::default_route_used("outbound");
        routing::evaluation_latency_seconds(0.001);

        auth::auth_attempt("sip");
        auth::auth_success("sip");
        auth::auth_failure("sip", "invalid_password");
    }
}
