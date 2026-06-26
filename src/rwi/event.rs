use serde::Serialize;
use tracing::warn;

use crate::rwi::proto::EventCallContext;

/// Type-erased RWI event for gateway dispatching.
/// No enum, no match — just fields. Everything past this point is polymorphic.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RwiEvent {
    pub event_type: &'static str,
    pub call_id: Option<String>,
    pub payload: serde_json::Value,
}

impl RwiEvent {
    /// Build from a typed RwiEventSpec. If context provided, merge it into payload.
    pub fn from_spec<E: RwiEventSpec>(event: &E, ctx: Option<&EventCallContext>) -> Self {
        let mut payload = serde_json::to_value(event).unwrap_or_else(|e| {
            warn!("RwiEventSpec serialization failed: {}", e);
            serde_json::json!({})
        });
        payload["event_type"] = serde_json::Value::String(E::TYPE.into());
        merge_event_context(&mut payload, ctx);
        RwiEvent {
            event_type: E::TYPE,
            call_id: event.call_id().map(|s| s.to_owned()),
            payload,
        }
    }
}

pub fn merge_event_context(payload: &mut serde_json::Value, ctx: Option<&EventCallContext>) {
    if let Some(ctx) = ctx {
        if let Ok(ctx_val) = serde_json::to_value(ctx) {
            if let (Some(pobj), Some(cobj)) = (payload.as_object_mut(), ctx_val.as_object()) {
                for (k, v) in cobj {
                    if !v.is_null() && !pobj.contains_key(k) {
                        pobj.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }
}

/// Trait for typed event structs. Each event type implements this.
pub trait RwiEventSpec: Serialize {
    const TYPE: &'static str;
    fn call_id(&self) -> Option<&str>;
}

/// Build a flat payload from a spec, optionally enriched with context.
pub fn to_flat_payload<E: RwiEventSpec>(
    event: &E,
    ctx: Option<&EventCallContext>,
) -> serde_json::Value {
    RwiEvent::from_spec(event, ctx).payload
}

/// Bridge a typed event spec into the legacy enum transport during migration.
pub fn to_legacy_event<E: RwiEventSpec>(event: &E, ctx: Option<&EventCallContext>) -> RwiEvent {
    RwiEvent::from_spec(event, ctx)
}

/// Macro to generate `RwiEventSpec` impls for events whose `call_id()`
/// returns `Some(&self.call_id)`.
macro_rules! rwi_event {
    ($ty:ident, $type:literal) => {
        impl RwiEventSpec for $ty {
            const TYPE: &'static str = $type;
            fn call_id(&self) -> Option<&str> {
                Some(&self.call_id)
            }
        }
    };
}

// ═══════════════════════════════════════════════════════════════════════
// Core event structs — Bucket A uses the rwi_event! macro.
// Special cases with different field names maintain manual impls.
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct CallIncoming {
    pub call_id: String,
    pub context: String,
    pub caller: String,
    pub callee: String,
    pub dial_direction: String,
    pub trunk: Option<String>,
    #[serde(default)]
    pub sip_headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub root_call_id: Option<String>,
    #[serde(default)]
    pub caller_name: Option<String>,
    #[serde(default)]
    pub callee_name: Option<String>,
    #[serde(default)]
    pub called_phone: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub routing_target: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub routing_path: Option<Vec<String>>,
}
rwi_event!(CallIncoming, "call_incoming");

#[derive(Debug, Clone, Serialize)]
pub struct CallRinging {
    pub call_id: String,
}
rwi_event!(CallRinging, "call_ringing");

#[derive(Debug, Clone, Serialize)]
pub struct CallAnswered {
    pub call_id: String,
}
rwi_event!(CallAnswered, "call_answered");

#[derive(Debug, Clone, Serialize)]
pub struct CallHangup {
    pub call_id: String,
    pub reason: Option<String>,
    pub sip_status: Option<u16>,
}
rwi_event!(CallHangup, "call_hangup");

#[derive(Debug, Clone, Serialize)]
pub struct CallEarlyMedia {
    pub call_id: String,
}
rwi_event!(CallEarlyMedia, "call_early_media");

#[derive(Debug, Clone, Serialize)]
pub struct CallNoAnswer {
    pub call_id: String,
}
rwi_event!(CallNoAnswer, "call_no_answer");

#[derive(Debug, Clone, Serialize)]
pub struct CallBusy {
    pub call_id: String,
}
rwi_event!(CallBusy, "call_busy");

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferred {
    pub call_id: String,
}
rwi_event!(CallTransferred, "call_transferred");

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferAccepted {
    pub call_id: String,
}
rwi_event!(CallTransferAccepted, "call_transfer_accepted");

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferFailed {
    pub call_id: String,
    pub sip_status: Option<u16>,
    pub reason: Option<String>,
}
rwi_event!(CallTransferFailed, "call_transfer_failed");

#[derive(Debug, Clone, Serialize)]
pub struct RecordStarted {
    pub call_id: String,
}
rwi_event!(RecordStarted, "record_started");

#[derive(Debug, Clone, Serialize)]
pub struct RecordStopped {
    pub call_id: String,
    pub duration_secs: Option<u64>,
    pub filename: Option<String>,
    pub unique_id: Option<String>,
    pub file_size: Option<u64>,
    pub download_url: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub called_phone: Option<String>,
    pub call_type: Option<String>,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub call_start_time: Option<String>,
    pub call_end_time: Option<String>,
    pub upload_time: Option<String>,
    pub switch_flag: Option<String>,
    pub root_call_id: Option<String>,
}
rwi_event!(RecordStopped, "record_stopped");

#[derive(Debug, Clone, Serialize)]
pub struct RecordEnd {
    pub call_id: String,
    pub url: Option<String>,
    pub duration_secs: u64,
    pub file_size: u64,
}
rwi_event!(RecordEnd, "record_end");

#[derive(Debug, Clone, Serialize)]
pub struct RecordingMetadataAvailable {
    pub call_id: String,
    pub metadata: crate::rwi::RecordingMetadata,
}
rwi_event!(RecordingMetadataAvailable, "recording_metadata_available");

#[derive(Debug, Clone, Serialize)]
pub struct RecordPaused {
    pub call_id: String,
}
rwi_event!(RecordPaused, "record_paused");

#[derive(Debug, Clone, Serialize)]
pub struct RecordResumed {
    pub call_id: String,
}
rwi_event!(RecordResumed, "record_resumed");

#[derive(Debug, Clone, Serialize)]
pub struct MediaHoldStarted {
    pub call_id: String,
}
rwi_event!(MediaHoldStarted, "media_hold_started");

#[derive(Debug, Clone, Serialize)]
pub struct MediaHoldStopped {
    pub call_id: String,
}
rwi_event!(MediaHoldStopped, "media_hold_stopped");

#[derive(Debug, Clone, Serialize)]
pub struct MediaRingbackPassthroughStarted {
    pub source: String,
    pub target: String,
}
impl RwiEventSpec for MediaRingbackPassthroughStarted {
    const TYPE: &'static str = "media_ringback_passthrough_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaPlayStarted {
    pub call_id: String,
    pub leg_id: Option<String>,
    pub track_id: String,
}
rwi_event!(MediaPlayStarted, "media_play_started");

#[derive(Debug, Clone, Serialize)]
pub struct MediaPlayFinished {
    pub call_id: String,
    pub leg_id: Option<String>,
    pub track_id: String,
    pub interrupted: bool,
}
rwi_event!(MediaPlayFinished, "media_play_finished");

#[derive(Debug, Clone, Serialize)]
pub struct Dtmf {
    pub call_id: String,
    pub digit: String,
    pub leg_id: Option<String>,
    pub extra: Option<serde_json::Value>,
}
rwi_event!(Dtmf, "dtmf");

#[derive(Debug, Clone, Serialize)]
pub struct CallBridged {
    pub leg_a: String,
    pub leg_b: String,
}
impl RwiEventSpec for CallBridged {
    const TYPE: &'static str = "call_bridged";
    fn call_id(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceCreated {
    pub conf_id: String,
}
impl RwiEventSpec for ConferenceCreated {
    const TYPE: &'static str = "conference_created";
    fn call_id(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueJoined {
    pub call_id: String,
    pub queue_id: String,
}
rwi_event!(QueueJoined, "queue_joined");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentOffered {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
}
rwi_event!(QueueAgentOffered, "queue_agent_offered");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentConnected {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
}
rwi_event!(QueueAgentConnected, "queue_agent_connected");

#[derive(Debug, Clone, Serialize)]
pub struct QueueLeft {
    pub call_id: String,
    pub queue_id: String,
    pub reason: Option<String>,
}
rwi_event!(QueueLeft, "queue_left");

#[derive(Debug, Clone, Serialize)]
pub struct QueueWaitTimeout {
    pub call_id: String,
    pub queue_id: String,
}
rwi_event!(QueueWaitTimeout, "queue_wait_timeout");

#[derive(Debug, Clone, Serialize)]
pub struct QueueOverflowed {
    pub call_id: String,
    pub original_queue_id: String,
    pub overflow_queue_id: String,
    pub reason: String,
}
rwi_event!(QueueOverflowed, "queue_overflowed");

#[derive(Debug, Clone, Serialize)]
pub struct QueueVoicemailRedirected {
    pub call_id: String,
    pub queue_id: String,
    pub reason: String,
}
rwi_event!(QueueVoicemailRedirected, "queue_voicemail_redirected");

#[derive(Debug, Clone, Serialize)]
pub struct QueueCandidatesFound {
    pub call_id: String,
    pub queue_id: String,
    pub candidates: Vec<String>,
    pub trace_id: String,
}
rwi_event!(QueueCandidatesFound, "queue_candidates_found");

#[derive(Debug, Clone, Serialize)]
pub struct QueueFallbackExecuted {
    pub call_id: String,
    pub queue_id: String,
    pub action: String,
    pub reason: String,
    pub trace_id: String,
}
rwi_event!(QueueFallbackExecuted, "queue_fallback_executed");

#[derive(Debug, Clone, Serialize)]
pub struct CallUnbridged {
    pub call_id: String,
}
rwi_event!(CallUnbridged, "call_unbridged");

#[derive(Debug, Clone, Serialize)]
pub struct DtmfCollected {
    pub call_id: String,
    pub leg_id: String,
    pub digits: String,
}
rwi_event!(DtmfCollected, "dtmf_collected");

#[derive(Debug, Clone, Serialize)]
pub struct DtmfCollectionTimeout {
    pub call_id: String,
    pub leg_id: String,
}
rwi_event!(DtmfCollectionTimeout, "dtmf_collection_timeout");

#[derive(Debug, Clone, Serialize)]
pub struct SipMessageReceived {
    pub call_id: String,
    pub content_type: String,
    pub body: String,
}
rwi_event!(SipMessageReceived, "sip_message_received");

#[derive(Debug, Clone, Serialize)]
pub struct SipNotifyReceived {
    pub call_id: String,
    pub event: String,
    pub content_type: String,
    pub body: String,
}
rwi_event!(SipNotifyReceived, "sip_notify_received");

#[derive(Debug, Clone, Serialize)]
pub struct MediaStreamStarted {
    pub call_id: String,
}
rwi_event!(MediaStreamStarted, "media_stream_started");

#[derive(Debug, Clone, Serialize)]
pub struct MediaStreamStopped {
    pub call_id: String,
}
rwi_event!(MediaStreamStopped, "media_stream_stopped");

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorListenStarted {
    pub supervisor_call_id: String,
    pub target_call_id: String,
}
impl RwiEventSpec for SupervisorListenStarted {
    const TYPE: &'static str = "supervisor_listen_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.target_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorWhisperStarted {
    pub supervisor_call_id: String,
    pub target_call_id: String,
}
impl RwiEventSpec for SupervisorWhisperStarted {
    const TYPE: &'static str = "supervisor_whisper_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.target_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorBargeStarted {
    pub supervisor_call_id: String,
    pub target_call_id: String,
}
impl RwiEventSpec for SupervisorBargeStarted {
    const TYPE: &'static str = "supervisor_barge_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.target_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorTakeoverStarted {
    pub supervisor_call_id: String,
    pub target_call_id: String,
}
impl RwiEventSpec for SupervisorTakeoverStarted {
    const TYPE: &'static str = "supervisor_takeover_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.target_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorModeStopped {
    pub supervisor_call_id: String,
    pub target_call_id: String,
}
impl RwiEventSpec for SupervisorModeStopped {
    const TYPE: &'static str = "supervisor_mode_stopped";
    fn call_id(&self) -> Option<&str> {
        Some(&self.target_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateStarted {
    pub operation_id: String,
    pub leg_count: u32,
}
impl RwiEventSpec for ParallelOriginateStarted {
    const TYPE: &'static str = "parallel_originate_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.operation_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateLegRinging {
    pub operation_id: String,
    pub call_id: String,
    pub destination: String,
}
rwi_event!(
    ParallelOriginateLegRinging,
    "parallel_originate_leg_ringing"
);

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateWinner {
    pub operation_id: String,
    pub call_id: String,
    pub destination: String,
}
impl RwiEventSpec for ParallelOriginateWinner {
    const TYPE: &'static str = "parallel_originate_winner";
    fn call_id(&self) -> Option<&str> {
        Some(&self.operation_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateLegCancelled {
    pub operation_id: String,
    pub call_id: String,
    pub reason: String,
}
impl RwiEventSpec for ParallelOriginateLegCancelled {
    const TYPE: &'static str = "parallel_originate_leg_cancelled";
    fn call_id(&self) -> Option<&str> {
        Some(&self.operation_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateCompleted {
    pub operation_id: String,
    pub winning_call_id: String,
}
impl RwiEventSpec for ParallelOriginateCompleted {
    const TYPE: &'static str = "parallel_originate_completed";
    fn call_id(&self) -> Option<&str> {
        Some(&self.operation_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateFailed {
    pub operation_id: String,
    pub reason: String,
}
impl RwiEventSpec for ParallelOriginateFailed {
    const TYPE: &'static str = "parallel_originate_failed";
    fn call_id(&self) -> Option<&str> {
        Some(&self.operation_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceError {
    pub conf_id: String,
    pub error: String,
}
impl RwiEventSpec for ConferenceError {
    const TYPE: &'static str = "conference_error";
    fn call_id(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberJoined {
    pub conf_id: String,
    pub call_id: String,
}
rwi_event!(ConferenceMemberJoined, "conference_member_joined");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberLeft {
    pub conf_id: String,
    pub call_id: String,
}
rwi_event!(ConferenceMemberLeft, "conference_member_left");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberMuted {
    pub conf_id: String,
    pub call_id: String,
}
rwi_event!(ConferenceMemberMuted, "conference_member_muted");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberUnmuted {
    pub conf_id: String,
    pub call_id: String,
}
rwi_event!(ConferenceMemberUnmuted, "conference_member_unmuted");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceDestroyed {
    pub conf_id: String,
}
impl RwiEventSpec for ConferenceDestroyed {
    const TYPE: &'static str = "conference_destroyed";
    fn call_id(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceEndedByHost {
    pub conf_id: String,
    pub host_call_id: String,
    pub removed_call_ids: Vec<String>,
}
impl RwiEventSpec for ConferenceEndedByHost {
    const TYPE: &'static str = "conference_ended_by_host";
    fn call_id(&self) -> Option<&str> {
        Some(&self.host_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMergeRequested {
    pub call_id: String,
    pub consultation_call_id: String,
}
rwi_event!(ConferenceMergeRequested, "conference_merge_requested");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMerged {
    pub conf_id: String,
    pub call_id: String,
}
rwi_event!(ConferenceMerged, "conference_merged");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMergeFailed {
    pub conf_id: String,
    pub call_id: String,
    pub reason: String,
}
rwi_event!(ConferenceMergeFailed, "conference_merge_failed");

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceSeatReplaceStarted {
    pub conf_id: String,
    pub old_call_id: String,
    pub new_call_id: String,
}
impl RwiEventSpec for ConferenceSeatReplaceStarted {
    const TYPE: &'static str = "conference_seat_replace_started";
    fn call_id(&self) -> Option<&str> {
        Some(&self.new_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceSeatReplaceSucceeded {
    pub conf_id: String,
    pub old_call_id: String,
    pub new_call_id: String,
}
impl RwiEventSpec for ConferenceSeatReplaceSucceeded {
    const TYPE: &'static str = "conference_seat_replace_succeeded";
    fn call_id(&self) -> Option<&str> {
        Some(&self.new_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceSeatReplaceFailed {
    pub conf_id: String,
    pub old_call_id: String,
    pub new_call_id: String,
    pub reason: String,
}
impl RwiEventSpec for ConferenceSeatReplaceFailed {
    const TYPE: &'static str = "conference_seat_replace_failed";
    fn call_id(&self) -> Option<&str> {
        Some(&self.new_call_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IvrNodeEntered {
    pub call_id: String,
    pub node_id: String,
    pub node_name: String,
    pub node_type: String,
    pub app_id: String,
    pub entry_time: String,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub routing_target: Option<String>,
    pub previous_node_id: Option<String>,
    pub extra: Option<serde_json::Value>,
}
rwi_event!(IvrNodeEntered, "ivr_node_entered");

#[derive(Debug, Clone, Serialize)]
pub struct IvrNodeExited {
    pub call_id: String,
    pub node_id: String,
    pub node_name: String,
    pub result_value: Option<String>,
    pub duration_ms: u32,
    pub exit_time: String,
    pub next_node_id: Option<String>,
    pub hangup_reason: Option<String>,
    pub call_result: Option<String>,
    pub extra: Option<serde_json::Value>,
}
rwi_event!(IvrNodeExited, "ivr_node_exited");

#[derive(Debug, Clone, Serialize)]
pub struct IvrFlowCompleted {
    pub call_id: String,
    pub app_id: String,
    pub total_nodes_traversed: u32,
    pub total_duration_ms: u32,
    pub final_result: String,
    pub completion_time: String,
    pub final_routing_target: Option<String>,
    pub extra: Option<serde_json::Value>,
}
rwi_event!(IvrFlowCompleted, "ivr_flow_completed");

#[derive(Debug, Clone, Serialize)]
pub struct IvrStepTrace {
    pub call_id: String,
    pub session_id: String,
    pub caller: String,
    pub callee: String,
    pub step_index: u32,
    pub event_type: String,
    pub event_detail: Option<String>,
    pub action_type: String,
    pub action_json: Option<String>,
    pub result_kind: String,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub step_id: Option<String>,
    pub step_name: Option<String>,
    pub step_start_time: Option<String>,
    pub step_end_time: Option<String>,
    pub extra: Option<serde_json::Value>,
    pub sip_headers: Option<std::collections::HashMap<String, String>>,
}
rwi_event!(IvrStepTrace, "ivr_step_trace");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ivr_events_serialize_extra_field() {
        let entered = to_flat_payload(
            &IvrNodeEntered {
                call_id: "call-1".into(),
                node_id: "root".into(),
                node_name: "Root".into(),
                node_type: "menu".into(),
                app_id: "ivr-main".into(),
                entry_time: "2026-01-01T00:00:00Z".into(),
                caller_name: None,
                callee_name: None,
                routing_target: None,
                previous_node_id: None,
                extra: None,
            },
            None,
        );
        let exited = to_flat_payload(
            &IvrNodeExited {
                call_id: "call-1".into(),
                node_id: "root".into(),
                node_name: "Root".into(),
                result_value: None,
                duration_ms: 10,
                exit_time: "2026-01-01T00:00:01Z".into(),
                next_node_id: None,
                hangup_reason: None,
                call_result: None,
                extra: None,
            },
            None,
        );
        let completed = to_flat_payload(
            &IvrFlowCompleted {
                call_id: "call-1".into(),
                app_id: "ivr-main".into(),
                total_nodes_traversed: 1,
                total_duration_ms: 10,
                final_result: "completed".into(),
                completion_time: "2026-01-01T00:00:02Z".into(),
                final_routing_target: None,
                extra: None,
            },
            None,
        );

        assert!(entered.get("extra").is_some());
        assert!(exited.get("extra").is_some());
        assert!(completed.get("extra").is_some());
    }

    #[test]
    fn dtmf_event_serialize_extra_field() {
        let dtmf = to_flat_payload(
            &Dtmf {
                call_id: "call-1".into(),
                digit: "5".into(),
                leg_id: None,
                extra: None,
            },
            None,
        );
        let dtmf_with_extra = to_flat_payload(
            &Dtmf {
                call_id: "call-2".into(),
                digit: "9".into(),
                leg_id: Some("caller".into()),
                extra: Some(serde_json::json!({"foo": "bar"})),
            },
            None,
        );

        assert!(dtmf.get("extra").is_some());
        assert_eq!(dtmf["extra"], serde_json::Value::Null);

        assert!(dtmf_with_extra.get("extra").is_some());
        assert_eq!(dtmf_with_extra["extra"]["foo"], "bar");
    }
}
