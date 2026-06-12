use serde::Serialize;

use crate::rwi::proto::EventCallContext;
use crate::rwi::proto::RwiEvent;

/// Type-erased RWI event for gateway dispatching.
/// No enum, no match — just fields. Everything past this point is polymorphic.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FlatEvent {
    pub event_type: &'static str,
    pub call_id: Option<String>,
    pub payload: serde_json::Value,
}

impl FlatEvent {
    /// Build from a typed RwiEventSpec. If context provided, merge it into payload.
    pub fn from_spec<E: RwiEventSpec>(event: &E, ctx: Option<&EventCallContext>) -> Self {
        let mut payload = serde_json::to_value(event).expect("RwiEventSpec must be Serialize");
        payload["event_type"] = serde_json::Value::String(E::TYPE.into());
        merge_event_context(&mut payload, ctx);
        FlatEvent { event_type: E::TYPE, call_id: event.call_id().map(|s| s.to_owned()), payload }
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
pub fn to_flat_payload<E: RwiEventSpec>(event: &E, ctx: Option<&EventCallContext>) -> serde_json::Value {
    FlatEvent::from_spec(event, ctx).payload
}

/// Bridge a typed event spec into the legacy enum transport during migration.
pub fn to_legacy_event<E: RwiEventSpec>(event: &E, ctx: Option<&EventCallContext>) -> RwiEvent {
    RwiEvent::Custom(FlatEvent::from_spec(event, ctx))
}

// ═══════════════════════════════════════════════════════════════════════
// Core event structs — each is a plain struct with manual RwiEventSpec
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
impl RwiEventSpec for CallIncoming {
    const TYPE: &'static str = "call_incoming";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallRinging {
    pub call_id: String,
}
impl RwiEventSpec for CallRinging {
    const TYPE: &'static str = "call_ringing";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallAnswered {
    pub call_id: String,
}
impl RwiEventSpec for CallAnswered {
    const TYPE: &'static str = "call_answered";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallHangup {
    pub call_id: String,
    pub reason: Option<String>,
    pub sip_status: Option<u16>,
}
impl RwiEventSpec for CallHangup {
    const TYPE: &'static str = "call_hangup";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallEarlyMedia {
    pub call_id: String,
}
impl RwiEventSpec for CallEarlyMedia {
    const TYPE: &'static str = "call_early_media";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallNoAnswer {
    pub call_id: String,
}
impl RwiEventSpec for CallNoAnswer {
    const TYPE: &'static str = "call_no_answer";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallBusy {
    pub call_id: String,
}
impl RwiEventSpec for CallBusy {
    const TYPE: &'static str = "call_busy";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferred {
    pub call_id: String,
}
impl RwiEventSpec for CallTransferred {
    const TYPE: &'static str = "call_transferred";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferAccepted {
    pub call_id: String,
}
impl RwiEventSpec for CallTransferAccepted {
    const TYPE: &'static str = "call_transfer_accepted";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferFailed {
    pub call_id: String,
    pub sip_status: Option<u16>,
    pub reason: Option<String>,
}
impl RwiEventSpec for CallTransferFailed {
    const TYPE: &'static str = "call_transfer_failed";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordStarted {
    pub call_id: String,
}
impl RwiEventSpec for RecordStarted {
    const TYPE: &'static str = "record_started";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

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
impl RwiEventSpec for RecordStopped {
    const TYPE: &'static str = "record_stopped";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordEnd {
    pub call_id: String,
    pub url: Option<String>,
    pub duration_secs: u64,
    pub file_size: u64,
}
impl RwiEventSpec for RecordEnd {
    const TYPE: &'static str = "record_end";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingMetadataAvailable {
    pub call_id: String,
    pub metadata: crate::rwi::RecordingMetadata,
}
impl RwiEventSpec for RecordingMetadataAvailable {
    const TYPE: &'static str = "recording_metadata_available";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordPaused {
    pub call_id: String,
}
impl RwiEventSpec for RecordPaused {
    const TYPE: &'static str = "record_paused";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordResumed {
    pub call_id: String,
}
impl RwiEventSpec for RecordResumed {
    const TYPE: &'static str = "record_resumed";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaHoldStarted {
    pub call_id: String,
}
impl RwiEventSpec for MediaHoldStarted {
    const TYPE: &'static str = "media_hold_started";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaHoldStopped {
    pub call_id: String,
}
impl RwiEventSpec for MediaHoldStopped {
    const TYPE: &'static str = "media_hold_stopped";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaRingbackPassthroughStarted {
    pub source: String,
    pub target: String,
}
impl RwiEventSpec for MediaRingbackPassthroughStarted {
    const TYPE: &'static str = "media_ringback_passthrough_started";
    fn call_id(&self) -> Option<&str> { Some(&self.source) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaPlayStarted {
    pub call_id: String,
    pub leg_id: Option<String>,
    pub track_id: String,
}
impl RwiEventSpec for MediaPlayStarted {
    const TYPE: &'static str = "media_play_started";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaPlayFinished {
    pub call_id: String,
    pub leg_id: Option<String>,
    pub track_id: String,
    pub interrupted: bool,
}
impl RwiEventSpec for MediaPlayFinished {
    const TYPE: &'static str = "media_play_finished";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct Dtmf {
    pub call_id: String,
    pub digit: String,
    pub leg_id: Option<String>,
}
impl RwiEventSpec for Dtmf {
    const TYPE: &'static str = "dtmf";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallBridged {
    pub leg_a: String,
    pub leg_b: String,
}
impl RwiEventSpec for CallBridged {
    const TYPE: &'static str = "call_bridged";
    fn call_id(&self) -> Option<&str> { None }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceCreated {
    pub conf_id: String,
}
impl RwiEventSpec for ConferenceCreated {
    const TYPE: &'static str = "conference_created";
    fn call_id(&self) -> Option<&str> { None }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueJoined {
    pub call_id: String,
    pub queue_id: String,
}
impl RwiEventSpec for QueueJoined {
    const TYPE: &'static str = "queue_joined";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentOffered {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
}
impl RwiEventSpec for QueueAgentOffered {
    const TYPE: &'static str = "queue_agent_offered";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentConnected {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
}
impl RwiEventSpec for QueueAgentConnected {
    const TYPE: &'static str = "queue_agent_connected";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueLeft {
    pub call_id: String,
    pub queue_id: String,
    pub reason: Option<String>,
}
impl RwiEventSpec for QueueLeft {
    const TYPE: &'static str = "queue_left";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueWaitTimeout {
    pub call_id: String,
    pub queue_id: String,
}
impl RwiEventSpec for QueueWaitTimeout {
    const TYPE: &'static str = "queue_wait_timeout";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueOverflowed {
    pub call_id: String,
    pub original_queue_id: String,
    pub overflow_queue_id: String,
    pub reason: String,
}
impl RwiEventSpec for QueueOverflowed {
    const TYPE: &'static str = "queue_overflowed";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueVoicemailRedirected {
    pub call_id: String,
    pub queue_id: String,
    pub reason: String,
}
impl RwiEventSpec for QueueVoicemailRedirected {
    const TYPE: &'static str = "queue_voicemail_redirected";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueCandidatesFound {
    pub call_id: String,
    pub queue_id: String,
    pub candidates: Vec<String>,
    pub trace_id: String,
}
impl RwiEventSpec for QueueCandidatesFound {
    const TYPE: &'static str = "queue_candidates_found";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueFallbackExecuted {
    pub call_id: String,
    pub queue_id: String,
    pub action: String,
    pub reason: String,
    pub trace_id: String,
}
impl RwiEventSpec for QueueFallbackExecuted {
    const TYPE: &'static str = "queue_fallback_executed";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallUnbridged { pub call_id: String }
impl RwiEventSpec for CallUnbridged {
    const TYPE: &'static str = "call_unbridged";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct DtmfCollected { pub call_id: String, pub leg_id: String, pub digits: String }
impl RwiEventSpec for DtmfCollected {
    const TYPE: &'static str = "dtmf_collected";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct DtmfCollectionTimeout { pub call_id: String, pub leg_id: String }
impl RwiEventSpec for DtmfCollectionTimeout {
    const TYPE: &'static str = "dtmf_collection_timeout";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SipMessageReceived { pub call_id: String, pub content_type: String, pub body: String }
impl RwiEventSpec for SipMessageReceived {
    const TYPE: &'static str = "sip_message_received";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SipNotifyReceived { pub call_id: String, pub event: String, pub content_type: String, pub body: String }
impl RwiEventSpec for SipNotifyReceived {
    const TYPE: &'static str = "sip_notify_received";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaStreamStarted { pub call_id: String }
impl RwiEventSpec for MediaStreamStarted {
    const TYPE: &'static str = "media_stream_started";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaStreamStopped { pub call_id: String }
impl RwiEventSpec for MediaStreamStopped {
    const TYPE: &'static str = "media_stream_stopped";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorListenStarted { pub supervisor_call_id: String, pub target_call_id: String }
impl RwiEventSpec for SupervisorListenStarted {
    const TYPE: &'static str = "supervisor_listen_started";
    fn call_id(&self) -> Option<&str> { Some(&self.target_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorWhisperStarted { pub supervisor_call_id: String, pub target_call_id: String }
impl RwiEventSpec for SupervisorWhisperStarted {
    const TYPE: &'static str = "supervisor_whisper_started";
    fn call_id(&self) -> Option<&str> { Some(&self.target_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorBargeStarted { pub supervisor_call_id: String, pub target_call_id: String }
impl RwiEventSpec for SupervisorBargeStarted {
    const TYPE: &'static str = "supervisor_barge_started";
    fn call_id(&self) -> Option<&str> { Some(&self.target_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorTakeoverStarted { pub supervisor_call_id: String, pub target_call_id: String }
impl RwiEventSpec for SupervisorTakeoverStarted {
    const TYPE: &'static str = "supervisor_takeover_started";
    fn call_id(&self) -> Option<&str> { Some(&self.target_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorModeStopped { pub supervisor_call_id: String, pub target_call_id: String }
impl RwiEventSpec for SupervisorModeStopped {
    const TYPE: &'static str = "supervisor_mode_stopped";
    fn call_id(&self) -> Option<&str> { Some(&self.target_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateStarted { pub operation_id: String, pub leg_count: u32 }
impl RwiEventSpec for ParallelOriginateStarted {
    const TYPE: &'static str = "parallel_originate_started";
    fn call_id(&self) -> Option<&str> { Some(&self.operation_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateLegRinging { pub operation_id: String, pub call_id: String, pub destination: String }
impl RwiEventSpec for ParallelOriginateLegRinging {
    const TYPE: &'static str = "parallel_originate_leg_ringing";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateWinner { pub operation_id: String, pub call_id: String, pub destination: String }
impl RwiEventSpec for ParallelOriginateWinner {
    const TYPE: &'static str = "parallel_originate_winner";
    fn call_id(&self) -> Option<&str> { Some(&self.operation_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateLegCancelled { pub operation_id: String, pub call_id: String, pub reason: String }
impl RwiEventSpec for ParallelOriginateLegCancelled {
    const TYPE: &'static str = "parallel_originate_leg_cancelled";
    fn call_id(&self) -> Option<&str> { Some(&self.operation_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateCompleted { pub operation_id: String, pub winning_call_id: String }
impl RwiEventSpec for ParallelOriginateCompleted {
    const TYPE: &'static str = "parallel_originate_completed";
    fn call_id(&self) -> Option<&str> { Some(&self.operation_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ParallelOriginateFailed { pub operation_id: String, pub reason: String }
impl RwiEventSpec for ParallelOriginateFailed {
    const TYPE: &'static str = "parallel_originate_failed";
    fn call_id(&self) -> Option<&str> { Some(&self.operation_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceError { pub conf_id: String, pub error: String }
impl RwiEventSpec for ConferenceError {
    const TYPE: &'static str = "conference_error";
    fn call_id(&self) -> Option<&str> { None }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberJoined { pub conf_id: String, pub call_id: String }
impl RwiEventSpec for ConferenceMemberJoined {
    const TYPE: &'static str = "conference_member_joined";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberLeft { pub conf_id: String, pub call_id: String }
impl RwiEventSpec for ConferenceMemberLeft {
    const TYPE: &'static str = "conference_member_left";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberMuted { pub conf_id: String, pub call_id: String }
impl RwiEventSpec for ConferenceMemberMuted {
    const TYPE: &'static str = "conference_member_muted";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMemberUnmuted { pub conf_id: String, pub call_id: String }
impl RwiEventSpec for ConferenceMemberUnmuted {
    const TYPE: &'static str = "conference_member_unmuted";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceDestroyed { pub conf_id: String }
impl RwiEventSpec for ConferenceDestroyed {
    const TYPE: &'static str = "conference_destroyed";
    fn call_id(&self) -> Option<&str> { None }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceEndedByHost { pub conf_id: String, pub host_call_id: String, pub removed_call_ids: Vec<String> }
impl RwiEventSpec for ConferenceEndedByHost {
    const TYPE: &'static str = "conference_ended_by_host";
    fn call_id(&self) -> Option<&str> { Some(&self.host_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMergeRequested { pub call_id: String, pub consultation_call_id: String }
impl RwiEventSpec for ConferenceMergeRequested {
    const TYPE: &'static str = "conference_merge_requested";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMerged { pub conf_id: String, pub call_id: String }
impl RwiEventSpec for ConferenceMerged {
    const TYPE: &'static str = "conference_merged";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceMergeFailed { pub conf_id: String, pub call_id: String, pub reason: String }
impl RwiEventSpec for ConferenceMergeFailed {
    const TYPE: &'static str = "conference_merge_failed";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceSeatReplaceStarted { pub conf_id: String, pub old_call_id: String, pub new_call_id: String }
impl RwiEventSpec for ConferenceSeatReplaceStarted {
    const TYPE: &'static str = "conference_seat_replace_started";
    fn call_id(&self) -> Option<&str> { Some(&self.new_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceSeatReplaceSucceeded { pub conf_id: String, pub old_call_id: String, pub new_call_id: String }
impl RwiEventSpec for ConferenceSeatReplaceSucceeded {
    const TYPE: &'static str = "conference_seat_replace_succeeded";
    fn call_id(&self) -> Option<&str> { Some(&self.new_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConferenceSeatReplaceFailed { pub conf_id: String, pub old_call_id: String, pub new_call_id: String, pub reason: String }
impl RwiEventSpec for ConferenceSeatReplaceFailed {
    const TYPE: &'static str = "conference_seat_replace_failed";
    fn call_id(&self) -> Option<&str> { Some(&self.new_call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct IvrNodeEntered {
    pub call_id: String, pub node_id: String, pub node_name: String, pub node_type: String,
    pub app_id: String, pub entry_time: String,
    pub caller_name: Option<String>, pub callee_name: Option<String>,
    pub routing_target: Option<String>, pub previous_node_id: Option<String>,
}
impl RwiEventSpec for IvrNodeEntered {
    const TYPE: &'static str = "ivr_node_entered";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct IvrNodeExited {
    pub call_id: String, pub node_id: String, pub node_name: String,
    pub result_value: Option<String>, pub duration_ms: u32, pub exit_time: String,
    pub next_node_id: Option<String>, pub hangup_reason: Option<String>,
    pub call_result: Option<String>,
}
impl RwiEventSpec for IvrNodeExited {
    const TYPE: &'static str = "ivr_node_exited";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct IvrFlowCompleted {
    pub call_id: String, pub app_id: String,
    pub total_nodes_traversed: u32, pub total_duration_ms: u32,
    pub final_result: String, pub completion_time: String,
    pub final_routing_target: Option<String>,
}
impl RwiEventSpec for IvrFlowCompleted {
    const TYPE: &'static str = "ivr_flow_completed";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}

#[derive(Debug, Clone, Serialize)]
pub struct IvrStepTrace {
    pub call_id: String, pub session_id: String,
    pub caller: String, pub callee: String,
    pub step_index: u32, pub event_type: String,
    pub event_detail: Option<String>,
    pub action_type: String, pub action_json: Option<String>,
    pub result_kind: String, pub duration_ms: u64,
    pub error: Option<String>,
    pub step_id: Option<String>, pub step_name: Option<String>,
    pub step_start_time: Option<String>, pub step_end_time: Option<String>,
    pub extra: Option<serde_json::Value>,
    pub sip_headers: Option<std::collections::HashMap<String, String>>,
}
impl RwiEventSpec for IvrStepTrace {
    const TYPE: &'static str = "ivr_step_trace";
    fn call_id(&self) -> Option<&str> { Some(&self.call_id) }
}
