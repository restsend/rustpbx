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

    pub fn invite_latency_seconds(duration_secs: f64, direction: &str) {
        metrics::histogram!(
            "rustpbx_sip_invite_latency_seconds",
            "direction" => direction.to_string()
        )
        .record(duration_secs);
    }
}

pub mod trunk {
    pub fn call_routed(trunk_id: &str, direction: &str) {
        metrics::counter!(
            "rustpbx_trunk_calls_total",
            "trunk_id" => trunk_id.to_string(),
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    pub fn call_failed(trunk_id: &str, direction: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_trunk_calls_failed_total",
            "trunk_id" => trunk_id.to_string(),
            "direction" => direction.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn latency_seconds(trunk_id: &str, duration_secs: f64) {
        metrics::histogram!(
            "rustpbx_trunk_latency_seconds",
            "trunk_id" => trunk_id.to_string()
        )
        .record(duration_secs);
    }

    pub fn set_status(trunk_id: &str, online: bool) {
        metrics::gauge!(
            "rustpbx_trunk_status",
            "trunk_id" => trunk_id.to_string()
        )
        .set(if online { 1.0 } else { 0.0 });
    }
}

pub mod media {
    pub fn rtp_packets_sent(count: u64, codec: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_sent_total",
            "codec" => codec.to_string()
        )
        .increment(count);
    }

    pub fn rtp_packets_received(count: u64, codec: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_received_total",
            "codec" => codec.to_string()
        )
        .increment(count);
    }

    pub fn rtp_packets_lost(count: u64, direction: &str) {
        metrics::counter!(
            "rustpbx_rtp_packets_lost_total",
            "direction" => direction.to_string()
        )
        .increment(count);
    }

    pub fn rtp_jitter_seconds(jitter_secs: f64, direction: &str) {
        metrics::histogram!(
            "rustpbx_rtp_jitter_seconds",
            "direction" => direction.to_string()
        )
        .record(jitter_secs);
    }

    pub fn set_codec_usage(codec: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_media_codec_usage",
            "codec" => codec.to_string()
        )
        .set(count as f64);
    }

    pub fn ice_connection_time_seconds(duration_secs: f64) {
        metrics::histogram!("rustpbx_webrtc_ice_connection_seconds").record(duration_secs);
    }

    pub fn webrtc_connection_created() {
        metrics::counter!("rustpbx_webrtc_connections_total").increment(1);
    }

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

    pub fn websocket_connection_created() {
        metrics::counter!("rustpbx_websocket_connections_total").increment(1);
    }

    pub fn websocket_connection_closed() {
        metrics::counter!("rustpbx_websocket_disconnections_total").increment(1);
    }

    pub fn set_websocket_connections(count: usize) {
        metrics::gauge!("rustpbx_websocket_connections_active").set(count as f64);
    }
}

pub mod voicemail {
    pub fn message_received(mailbox: &str) {
        metrics::counter!(
            "rustpbx_voicemail_messages_total",
            "mailbox" => mailbox.to_string()
        )
        .increment(1);
    }

    pub fn message_duration_seconds(duration_secs: f64, mailbox: &str) {
        metrics::histogram!(
            "rustpbx_voicemail_duration_seconds",
            "mailbox" => mailbox.to_string()
        )
        .record(duration_secs);
    }

    pub fn set_message_count(mailbox: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_voicemail_messages_stored",
            "mailbox" => mailbox.to_string()
        )
        .set(count as f64);
    }
}

pub mod queue {
    pub fn wait_time_seconds(duration_secs: f64, queue_name: &str) {
        metrics::histogram!(
            "rustpbx_queue_wait_time_seconds",
            "queue" => queue_name.to_string()
        )
        .record(duration_secs);
    }

    pub fn set_size(queue_name: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_queue_size",
            "queue" => queue_name.to_string()
        )
        .set(count as f64);
    }

    pub fn caller_abandoned(queue_name: &str) {
        metrics::counter!(
            "rustpbx_queue_abandoned_total",
            "queue" => queue_name.to_string()
        )
        .increment(1);
    }

    pub fn caller_answered(queue_name: &str) {
        metrics::counter!(
            "rustpbx_queue_answered_total",
            "queue" => queue_name.to_string()
        )
        .increment(1);
    }
}

pub mod transcription {
    pub fn request_received(language: &str) {
        metrics::counter!(
            "rustpbx_transcription_requests_total",
            "language" => language.to_string()
        )
        .increment(1);
    }

    pub fn request_succeeded(language: &str) {
        metrics::counter!(
            "rustpbx_transcription_success_total",
            "language" => language.to_string()
        )
        .increment(1);
    }

    pub fn request_failed(language: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_transcription_failed_total",
            "language" => language.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn latency_seconds(duration_secs: f64, language: &str) {
        metrics::histogram!(
            "rustpbx_transcription_latency_seconds",
            "language" => language.to_string()
        )
        .record(duration_secs);
    }

    pub fn audio_duration_seconds(duration_secs: f64, language: &str) {
        metrics::histogram!(
            "rustpbx_transcription_audio_seconds",
            "language" => language.to_string()
        )
        .record(duration_secs);
    }
}

pub mod routing {
    pub fn route_evaluated(direction: &str, matched: bool) {
        metrics::counter!(
            "rustpbx_routing_evaluations_total",
            "direction" => direction.to_string(),
            "matched" => matched.to_string()
        )
        .increment(1);
    }

    pub fn default_route_used(direction: &str) {
        metrics::counter!(
            "rustpbx_routing_default_route_total",
            "direction" => direction.to_string()
        )
        .increment(1);
    }

    pub fn evaluation_latency_seconds(duration_secs: f64) {
        metrics::histogram!("rustpbx_routing_evaluation_seconds").record(duration_secs);
    }
}

pub mod auth {
    pub fn auth_attempt(method: &str) {
        metrics::counter!(
            "rustpbx_auth_attempts_total",
            "method" => method.to_string()
        )
        .increment(1);
    }

    pub fn auth_success(method: &str) {
        metrics::counter!(
            "rustpbx_auth_success_total",
            "method" => method.to_string()
        )
        .increment(1);
    }

    pub fn auth_failure(method: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_auth_failure_total",
            "method" => method.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
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

    pub fn duration_seconds(duration_secs: f64, mode: &str) {
        metrics::histogram!(
            "rustpbx_transfer_duration_seconds",
            "mode" => mode.to_string()
        )
        .record(duration_secs);
    }

    pub fn set_active_transfers(count: usize) {
        metrics::gauge!("rustpbx_transfer_active").set(count as f64);
    }

    pub fn refer_received() {
        metrics::counter!("rustpbx_transfer_refer_received_total").increment(1);
    }

    pub fn refer_accepted() {
        metrics::counter!("rustpbx_transfer_refer_accepted_total").increment(1);
    }

    pub fn refer_rejected(reason: &str) {
        metrics::counter!(
            "rustpbx_transfer_refer_rejected_total",
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn three_pcc_fallback_triggered() {
        metrics::counter!("rustpbx_transfer_3pcc_fallback_total").increment(1);
    }

    pub fn three_pcc_success() {
        metrics::counter!("rustpbx_transfer_3pcc_success_total").increment(1);
    }

    pub fn three_pcc_failed(reason: &str) {
        metrics::counter!(
            "rustpbx_transfer_3pcc_failed_total",
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn notify_latency_seconds(duration_secs: f64) {
        metrics::histogram!("rustpbx_transfer_notify_latency_seconds").record(duration_secs);
    }

    pub fn attended_consult_initiated() {
        metrics::counter!("rustpbx_transfer_attended_consult_total").increment(1);
    }

    pub fn attended_completed() {
        metrics::counter!("rustpbx_transfer_attended_completed_total").increment(1);
    }

    pub fn attended_cancelled() {
        metrics::counter!("rustpbx_transfer_attended_cancelled_total").increment(1);
    }
}

pub mod conference {
    pub fn created() {
        metrics::counter!("rustpbx_conference_created_total").increment(1);
    }

    pub fn destroyed(reason: &str) {
        metrics::counter!(
            "rustpbx_conference_destroyed_total",
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn set_participants(conference_id: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_conference_participants",
            "conference" => conference_id.to_string()
        )
        .set(count as f64);
    }

    pub fn media_injected_bytes(conference_id: &str, bytes: u64) {
        metrics::counter!(
            "rustpbx_conference_media_injected_bytes_total",
            "conference" => conference_id.to_string()
        )
        .increment(bytes);
    }
}

pub mod cc {
    // ===== Queue Metrics =====
    pub fn queue_call_enqueued(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_queue_calls_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn queue_call_answered(queue_id: &str, wait_secs: f64) {
        metrics::counter!(
            "rustpbx_cc_queue_calls_answered_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
        metrics::histogram!(
            "rustpbx_cc_queue_wait_time_seconds",
            "queue" => queue_id.to_string()
        )
        .record(wait_secs);
    }

    pub fn queue_call_abandoned(queue_id: &str, wait_secs: f64) {
        metrics::counter!(
            "rustpbx_cc_queue_calls_abandoned_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
        metrics::histogram!(
            "rustpbx_cc_queue_wait_time_seconds",
            "queue" => queue_id.to_string()
        )
        .record(wait_secs);
    }

    pub fn queue_call_transferred(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_queue_calls_transferred_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn queue_call_voicemail(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_queue_calls_voicemail_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn queue_call_handle_time(queue_id: &str, handle_secs: f64) {
        metrics::histogram!(
            "rustpbx_cc_queue_handle_time_seconds",
            "queue" => queue_id.to_string()
        )
        .record(handle_secs);
    }

    pub fn set_queue_size(queue_id: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_cc_queue_size",
            "queue" => queue_id.to_string()
        )
        .set(count as f64);
    }

    pub fn set_queue_longest_wait(queue_id: &str, secs: u64) {
        metrics::gauge!(
            "rustpbx_cc_queue_longest_wait_seconds",
            "queue" => queue_id.to_string()
        )
        .set(secs as f64);
    }

    pub fn set_queue_sla_percentage(queue_id: &str, percentage: f64) {
        metrics::gauge!(
            "rustpbx_cc_queue_sla_percentage",
            "queue" => queue_id.to_string()
        )
        .set(percentage);
    }

    // ===== Agent Metrics =====
    pub fn agent_status_changed(agent_id: &str, status: &str) {
        metrics::gauge!(
            "rustpbx_cc_agent_status",
            "agent" => agent_id.to_string(),
            "status" => status.to_string()
        )
        .set(1.0);
    }

    pub fn agent_call_handled(agent_id: &str) {
        metrics::counter!(
            "rustpbx_cc_agent_calls_handled_total",
            "agent" => agent_id.to_string()
        )
        .increment(1);
    }

    pub fn agent_talk_time(agent_id: &str, secs: f64) {
        metrics::histogram!(
            "rustpbx_cc_agent_talk_time_seconds",
            "agent" => agent_id.to_string()
        )
        .record(secs);
    }

    pub fn agent_wrapup_time(agent_id: &str, secs: f64) {
        metrics::histogram!(
            "rustpbx_cc_agent_wrapup_time_seconds",
            "agent" => agent_id.to_string()
        )
        .record(secs);
    }

    pub fn agent_ringing_time(agent_id: &str, secs: f64) {
        metrics::histogram!(
            "rustpbx_cc_agent_ringing_time_seconds",
            "agent" => agent_id.to_string()
        )
        .record(secs);
    }

    pub fn set_agent_concurrent_calls(agent_id: &str, count: u32) {
        metrics::gauge!(
            "rustpbx_cc_agent_concurrent_calls",
            "agent" => agent_id.to_string()
        )
        .set(count as f64);
    }

    pub fn set_agents_by_status(status: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_cc_agents_active",
            "status" => status.to_string()
        )
        .set(count as f64);
    }

    // ===== SLA Metrics =====
    pub fn sla_answered_within_target(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_sla_answered_within_target_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn sla_answered_after_target(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_sla_answered_after_target_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn sla_breached(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_sla_breached_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn sla_alert_triggered(queue_id: &str, action_type: &str) {
        metrics::counter!(
            "rustpbx_cc_sla_alert_total",
            "queue" => queue_id.to_string(),
            "action" => action_type.to_string()
        )
        .increment(1);
    }

    // ===== Transfer Metrics =====
    pub fn transfer_consult_initiated(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_transfer_consult_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn transfer_consult_success(queue_id: &str) {
        metrics::counter!(
            "rustpbx_cc_transfer_consult_success_total",
            "queue" => queue_id.to_string()
        )
        .increment(1);
    }

    pub fn transfer_consult_failed(queue_id: &str, reason: &str) {
        metrics::counter!(
            "rustpbx_cc_transfer_consult_failed_total",
            "queue" => queue_id.to_string(),
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    // ===== Conference Metrics =====
    pub fn conference_created() {
        metrics::counter!("rustpbx_cc_conference_created_total").increment(1);
    }

    pub fn conference_destroyed(reason: &str) {
        metrics::counter!(
            "rustpbx_cc_conference_destroyed_total",
            "reason" => reason.to_string()
        )
        .increment(1);
    }

    pub fn set_conference_participants(conference_id: &str, count: usize) {
        metrics::gauge!(
            "rustpbx_cc_conference_participants",
            "conference" => conference_id.to_string()
        )
        .set(count as f64);
    }

    pub fn conference_media_injected_bytes(conference_id: &str, bytes: u64) {
        metrics::counter!(
            "rustpbx_cc_conference_media_injected_bytes_total",
            "conference" => conference_id.to_string()
        )
        .increment(bytes);
    }

    // ===== SIP MESSAGE Metrics =====
    pub fn sip_message_sent(agent_id: &str, status: &str) {
        metrics::counter!(
            "rustpbx_cc_sip_message_sent_total",
            "agent" => agent_id.to_string(),
            "status" => status.to_string()
        )
        .increment(1);
    }

    // ===== Autonomous Mode Metrics =====
    pub fn set_autonomous_mode(enabled: bool) {
        metrics::gauge!("rustpbx_cc_autonomous_mode_enabled")
            .set(if enabled { 1.0 } else { 0.0 });
    }

    pub fn autonomous_decision(decision_type: &str) {
        metrics::counter!(
            "rustpbx_cc_autonomous_decisions_total",
            "type" => decision_type.to_string()
        )
        .increment(1);
    }

    // ===== No-Answer Action Metrics =====
    pub fn no_answer_action_executed(queue_id: &str, action: &str) {
        metrics::counter!(
            "rustpbx_cc_no_answer_action_total",
            "queue" => queue_id.to_string(),
            "action" => action.to_string()
        )
        .increment(1);
    }
}

pub fn init_static_gauges() {
    let version = crate::version::get_short_version();
    metrics::gauge!("rustpbx_info", "version" => version).set(1.0);

    system::set_uptime_seconds();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_macros_compile() {
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

        transfer::attempt_total("refer", "blind");
        transfer::attempt_total("3pcc", "blind");
        transfer::attempt_total("attended", "attended");
        transfer::success_total("refer");
        transfer::success_total("3pcc");
        transfer::failed_total("refer", "timeout");
        transfer::failed_total("3pcc", "originate_failed");
        transfer::duration_seconds(5.0, "refer");
        transfer::set_active_transfers(3);
        transfer::refer_received();
        transfer::refer_accepted();
        transfer::refer_rejected("method_not_allowed");
        transfer::three_pcc_fallback_triggered();
        transfer::three_pcc_success();
        transfer::three_pcc_failed("bridge_failed");
        transfer::notify_latency_seconds(2.0);
        transfer::attended_consult_initiated();
        transfer::attended_completed();
        transfer::attended_cancelled();

        cc::queue_call_enqueued("support");
        cc::queue_call_answered("support", 15.0);
        cc::queue_call_abandoned("support", 30.0);
        cc::queue_call_transferred("support");
        cc::queue_call_voicemail("support");
        cc::queue_call_handle_time("support", 120.0);
        cc::set_queue_size("support", 3);
        cc::set_queue_longest_wait("support", 45);
        cc::set_queue_sla_percentage("support", 0.85);

        cc::agent_status_changed("agent-1", "idle");
        cc::agent_call_handled("agent-1");
        cc::agent_talk_time("agent-1", 60.0);
        cc::agent_wrapup_time("agent-1", 10.0);
        cc::agent_ringing_time("agent-1", 5.0);
        cc::set_agent_concurrent_calls("agent-1", 1);
        cc::set_agents_by_status("idle", 5);

        cc::sla_answered_within_target("support");
        cc::sla_answered_after_target("support");
        cc::sla_breached("support");
        cc::sla_alert_triggered("support", "webhook");

        cc::transfer_consult_initiated("support");
        cc::transfer_consult_success("support");
        cc::transfer_consult_failed("support", "timeout");

        cc::conference_created();
        cc::conference_destroyed("p2p_fallback");
        cc::set_conference_participants("conf-1", 3);
        cc::conference_media_injected_bytes("conf-1", 1024);

        cc::sip_message_sent("agent-1", "success");

        cc::set_autonomous_mode(true);
        cc::autonomous_decision("assign_agent");

        cc::no_answer_action_executed("support", "voicemail");
    }
}
