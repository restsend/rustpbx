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
#[cfg(test)]
pub fn to_flat_payload<E: RwiEventSpec>(
    event: &E,
    ctx: Option<&EventCallContext>,
) -> serde_json::Value {
    RwiEvent::from_spec(event, ctx).payload
}

/// Build a type-erased `RwiEvent` from a typed spec (optionally enriched with
/// call context). Alias for [`RwiEvent::from_spec`].
///
/// Retained for addons that translate heterogeneous internal events into
/// `RwiEvent` values for the RWI gateway / webhook (e.g. the CC addon's queue,
/// skill-group and agent event translators).
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

/// A call was created and entered the dialing (calling) phase — emitted for
/// inbound INVITEs and API originates alike. Direction is not part of the
/// struct: it is injected via `EventCallContext.direction` enrichment.
#[derive(Debug, Clone, Serialize)]
pub struct CallCreated {
    pub call_id: String,
    pub context: String,
    pub caller: String,
    pub callee: String,
    pub trunk: Option<String>,
    #[serde(default)]
    pub sip_headers: std::collections::HashMap<String, String>,
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
rwi_event!(CallCreated, "call_created");

#[derive(Debug, Clone, Serialize)]
pub struct CallRinging {
    /// Present for an individual leg event; absent for a session event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    pub call_id: String,
    /// `true` when the ringing provisional response carried SDP (183 Session
    /// Progress or 180 with SDP — early media), `false` for a plain 180
    /// Ringing. Emitted once per provisional response; consumers tell
    /// ringback from early media by this flag instead of a separate event.
    #[serde(default)]
    pub early_media: bool,
}
rwi_event!(CallRinging, "call_ringing");

#[derive(Debug, Clone, Serialize)]
pub struct CallAnswered {
    /// Present for an individual leg event; absent for a session event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    pub call_id: String,
}
rwi_event!(CallAnswered, "call_answered");

#[derive(Debug, Clone, Serialize)]
pub struct CallHangup {
    /// Present for an individual leg event; absent for a session event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    pub call_id: String,
    pub reason: Option<String>,
    /// Normalized initiator: `"agent"` | `"caller"` | `"system"` | `"transfer"`
    /// | `"unknown"`. Consistent with the former CC-layer `cc_hangup.hangup_by`.
    pub hangup_by: Option<String>,
    pub sip_status: Option<u16>,
    /// Talk time in seconds (answer → hangup). `None` when the call was never
    /// answered (originate setup failures). Replaces the `duration_secs`
    /// previously carried only by `cc_hangup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
}
rwi_event!(CallHangup, "call_hangup");

/// A call leg was put on hold (re-INVITE with `sendonly`/`inactive`, or an
/// explicit Hold command). Agent attribution (`agent_id` / `agent_name`) is
/// injected via `EventCallContext` enrichment when a CC agent participates —
/// replaces the former `cc_held` event.
#[derive(Debug, Clone, Serialize)]
pub struct CallHeld {
    pub call_id: String,
    pub leg_id: String,
}
rwi_event!(CallHeld, "call_held");

/// A previously held call leg was retrieved (resume). Replaces the former
/// `cc_unheld` event.
#[derive(Debug, Clone, Serialize)]
pub struct CallUnheld {
    pub call_id: String,
    pub leg_id: String,
}
rwi_event!(CallUnheld, "call_unheld");

/// The full user data object of a call session was replaced (REST
/// `PUT /calls/active/{session_id}/userdata` or RWI `call.set_userdata`).
/// Carries the complete new object — consumers track changes by replacing
/// their local copy, there is no partial merge. The new value also rides
/// every subsequent call-scoped event under `user_data` (enrichment) and is
/// persisted into the CDR `metadata["user_data"]`.
#[derive(Debug, Clone, Serialize)]
pub struct CallUserDataUpdated {
    pub call_id: String,
    pub user_data: serde_json::Value,
}
rwi_event!(CallUserDataUpdated, "call_userdata_updated");

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

/// Unified call-affecting error event. One event type for every subsystem that
/// can degrade, reject, or fail a call (routing, REST calls on the call path,
/// step IVR, TTS, queue/CC, auth/ACL, locator, ...). It is emitted alongside an
/// `error!`/`warn!`/`info!` log line (level follows `severity`) and a matching
/// `TraceKind::Error` entry in the CDR `metadata["trace"]`.
#[derive(Debug, Clone, Serialize)]
pub struct CallError {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Subsystem stage this failure belongs to. Stable values include
    /// `routing`, `rest_api`, `ivr_step`, `tts`, `queue`, `cc`, `auth`,
    /// `acl`, `locator`, `media`, `conference`.
    pub stage: String,
    /// Owning subsystem from the standardized error registry
    /// (`CallErrInfo::app`), e.g. `queue`, `tts`, `http_router`.
    pub app: String,
    /// Stable hierarchical registry code, e.g. `tts.synthesis_failed`.
    pub code: String,
    /// `info` | `warn` | `error` (mirrors `ErrSeverity`, drives log level).
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sip_status: Option<u16>,
    /// Structured runtime detail (targets, url, attempts, agent ids, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}
rwi_event!(CallError, "call_error");

impl CallError {
    /// Build from a standardized registry entry plus runtime detail.
    pub fn from_info(
        call_id: impl Into<String>,
        stage: &str,
        info: &'static crate::call_errors::CallErrInfo,
        detail: Option<serde_json::Value>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            session_id: None,
            stage: stage.to_string(),
            app: info.app.to_string(),
            code: info.code.to_string(),
            severity: info.severity.as_str().to_string(),
            message: info.message.to_string(),
            sip_status: info.sip_status,
            detail,
        }
    }
}

/// A database write failed. Emitted for background/async DB writes whose
/// failure would otherwise be silent (cluster session registry, queue
/// persistence, presence, locator cleanup, ...); call-scoped failures
/// additionally land a `TraceKind::Error` entry in the CDR trace at the
/// call site. `call_id` is present when the failed write belongs to a live
/// call, `None` for cluster/system tables.
#[derive(Debug, Clone, Serialize)]
pub struct DbWriteFailed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Table / model that failed to persist, e.g. `cluster_sessions`.
    pub entity: String,
    /// Coarse verb: `insert` | `update` | `delete` | `upsert` | `sweep`.
    pub operation: String,
    pub error: String,
    /// Failures suppressed by the per-(entity, operation) throttle since the
    /// last emission — lets consumers gauge the true failure rate during an
    /// outage window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<u64>,
}

impl RwiEventSpec for DbWriteFailed {
    const TYPE: &'static str = "db_write_failed";
    fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }
}

/// Where a transfer originated from — the flow position of the call at the
/// moment it was transferred (IVR / queue / agent / SIP REFER), so consumers
/// can reconstruct the call path from RWI events alone.
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct TransferSource {
    /// `ivr` | `queue` | `agent` | `refer` | `external`
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ivr_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Display name of the attributed agent, when known (CC-registered
    /// agents only — captured from the session's agent context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferred {
    pub call_id: String,
    pub transfer_target: Option<String>,
    /// Resolved target kind: `queue` | `ivr` | `route_point` | `voicemail` |
    /// `conference` | `bridge` | `sip`. `None` when unknown (e.g. a Replaces
    /// takeover where the target is opaque).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_target_type: Option<String>,
    /// Flow origin captured at transfer time: the IVR (and node) or queue the
    /// call was in, or the agent / REFER party that initiated the transfer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_source: Option<TransferSource>,
}
rwi_event!(CallTransferred, "call_transferred");

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferAccepted {
    pub call_id: String,
    pub transfer_target: Option<String>,
    /// Resolved target kind, same vocabulary as `CallTransferred`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_target_type: Option<String>,
}
rwi_event!(CallTransferAccepted, "call_transfer_accepted");

#[derive(Debug, Clone, Serialize)]
pub struct ConsultSwitched {
    pub call_id: String,
    pub transfer_id: String,
    /// `customer` or `consult`
    pub talking_to: String,
}
rwi_event!(ConsultSwitched, "consult_switched");

/// Customer DTMF response to a conference-authorization IVR
/// (`authorized` / `denied` / `timeout`).
#[derive(Debug, Clone, Serialize)]
pub struct ConferenceAuthResult {
    pub call_id: String,
    pub transfer_id: String,
    /// `authorized`, `denied`, or `timeout`
    pub result: String,
}
rwi_event!(ConferenceAuthResult, "conference_auth_result");

#[derive(Debug, Clone, Serialize)]
pub struct CallTransferFailed {
    pub call_id: String,
    pub sip_status: Option<u16>,
    pub reason: Option<String>,
    pub transfer_target: Option<String>,
    /// Resolved target kind, same vocabulary as `CallTransferred`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_target_type: Option<String>,
}
rwi_event!(CallTransferFailed, "call_transfer_failed");

#[derive(Debug, Clone, Serialize)]
pub struct RecordStarted {
    pub call_id: String,
    /// Recording-level unique identifier (UUID v4). Shared with the matching
    /// `record_stopped` and `recording_metadata_available` events so
    /// consumers can reconcile one recording across events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_id: Option<String>,
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
pub struct TranscriptStarted {
    pub call_id: String,
    /// Which sides carry their own ASR stream ("caller", "callee").
    pub sides: Vec<String>,
    /// Provider identifier (e.g. "deepgram").
    pub provider: Option<String>,
}
rwi_event!(TranscriptStarted, "transcript_started");

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptSegmentEvent {
    pub call_id: String,
    /// "caller" | "callee"
    pub side: String,
    pub text: String,
    /// `true` for interim hypotheses, `false` for finalized utterances.
    pub partial: bool,
    pub start_ms: u64,
    pub end_ms: u64,
    pub lang: Option<String>,
}
rwi_event!(TranscriptSegmentEvent, "transcript_segment");

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptError {
    pub call_id: String,
    /// "caller" | "callee" | null (whole provider)
    pub side: Option<String>,
    pub error: String,
}
rwi_event!(TranscriptError, "transcript_error");

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEnded {
    pub call_id: String,
    /// Why transcription stopped ("stopped", "call_ended", "error", ...).
    pub reason: String,
}
rwi_event!(TranscriptEnded, "transcript_ended");

/// One finalized utterance, published as its own event type so webhook
/// consumers can filter server-side via `[rwi_webhook] events =
/// ["transcript_final"]` without receiving partial (`transcript_segment`)
/// hypotheses. Emitted in addition to the corresponding `transcript_segment`
/// event (`partial == false`); fields are identical plus `provider`.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptFinal {
    pub call_id: String,
    /// "caller" | "callee"
    pub side: String,
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub lang: Option<String>,
    /// Provider identifier (e.g. "deepgram").
    pub provider: Option<String>,
}
rwi_event!(TranscriptFinal, "transcript_final");

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
pub struct QueueAgentNoAnswer {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
    pub attempt: u32,
}
rwi_event!(QueueAgentNoAnswer, "queue_agent_no_answer");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentRejected {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
    pub attempt: u32,
}
rwi_event!(QueueAgentRejected, "queue_agent_rejected");

#[derive(Debug, Clone, Serialize)]
pub struct QueuePositionChanged {
    pub call_id: String,
    pub queue_id: String,
    pub position: usize,
}
rwi_event!(QueuePositionChanged, "queue_position_changed");

/// The call was queued into an overflow (escalation) skill group in addition
/// to (cumulative/replace modes) or instead of (sequential mode) its original
/// queue. Emitted by both the core queue app and the ACD engine when an
/// overflow/escalation step joins a new group.
#[derive(Debug, Clone, Serialize)]
pub struct QueueOverflowJoined {
    pub call_id: String,
    /// Queue id the call originally joined (`queue_joined` source).
    pub queue_id: String,
    /// Overflow / escalation skill group just joined.
    pub skill_group: String,
}
rwi_event!(QueueOverflowJoined, "queue_overflow_joined");

#[derive(Debug, Clone, Serialize)]
pub struct QueueLeft {
    pub call_id: String,
    pub queue_id: String,
    pub reason: Option<String>,
    /// All skill groups the call was queued in during this queue entry
    /// (primary first, overflow/escalation groups in join order). `None`
    /// (omitted from the payload) when the queue is not skill-group routed —
    /// keeps the wire format backward compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_groups: Option<Vec<String>>,
}
rwi_event!(QueueLeft, "queue_left");

/// An event from a realtime (AI voice) WebSocket bridge — transcripts,
/// function calls, barge-in and lifecycle signals (`realtime_event`).
///
/// The core is a transport: `kind`/`data` passthrough for the business side;
/// credentials and prompts never appear here.
#[derive(Debug, Clone, Serialize)]
pub struct RealtimeEvent {
    pub call_id: String,
    /// Event kind: `connected` / `session_ready` / `speech_started` /
    /// `speech_stopped` / `barge_in` / `transcript_delta` / `transcript_final` /
    /// `function_call` / `error` / `disconnected`.
    pub kind: String,
    /// Kind-specific payload (e.g. `{"text": …}` for transcripts,
    /// `{"name": …, "arguments": …}` for function calls).
    #[serde(default)]
    pub data: serde_json::Value,
}
rwi_event!(RealtimeEvent, "realtime_event");

#[derive(Debug, Clone, Serialize)]
pub struct QueueWaitTimeout {
    pub call_id: String,
    pub queue_id: String,
}
rwi_event!(QueueWaitTimeout, "queue_wait_timeout");

#[derive(Debug, Clone, Serialize)]
pub struct QueueCandidatesFound {
    pub call_id: String,
    pub queue_id: String,
    pub candidates: Vec<String>,
}
rwi_event!(QueueCandidatesFound, "queue_candidates_found");

#[derive(Debug, Clone, Serialize)]
pub struct QueueFallbackExecuted {
    pub call_id: String,
    pub queue_id: String,
    pub action: String,
    pub reason: String,
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

/// A call leg joined a conference room (room dial-in via app=conference).
#[derive(Debug, Clone, Serialize)]
pub struct ConferenceJoined {
    pub conf_id: String,
    pub call_id: String,
    pub leg_id: String,
}
rwi_event!(ConferenceJoined, "conference_joined");

/// A call leg left a conference room (hangup or room teardown).
#[derive(Debug, Clone, Serialize)]
pub struct ConferenceLeft {
    pub conf_id: String,
    pub call_id: String,
    pub leg_id: String,
}
rwi_event!(ConferenceLeft, "conference_left");

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
    /// Terminal marker: `true` when this node's action ended the flow
    /// (hangup / transfer / jump / exit) or the session terminated on it.
    /// `None` when the flow continues into a successor node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<bool>,
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
    /// Always `true` — this event itself is the flow's end marker; the field
    /// lets consumers filter uniformly on the `end` flag across IVR events.
    pub end: bool,
    pub extra: Option<serde_json::Value>,
}
rwi_event!(IvrFlowCompleted, "ivr_flow_completed");

/// Structured trigger info describing what caused an IVR step to execute.
///
/// Serialized as a nested object: `{"type": "dtmf", "detail": {"digit": "2"}}`.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerInfo {
    /// Trigger source type, e.g. `dtmf`, `dtmf_menu`, `audio_complete`,
    /// `session_start`, `action_execute`, `chained`, `dtmf_menu_timeout`, ...
    #[serde(rename = "type")]
    pub r#type: String,
    /// Structured detail for the trigger. For DTMF this is `{"digit": "2"}`,
    /// for an API response `{"status": 200}`, for phone collection
    /// `{"number": "..."}`. `None` when the trigger carries no detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl TriggerInfo {
    pub fn new(r#type: impl Into<String>) -> Self {
        Self {
            r#type: r#type.into(),
            detail: None,
        }
    }

    pub fn with_detail(r#type: impl Into<String>, detail: serde_json::Value) -> Self {
        Self {
            r#type: r#type.into(),
            detail: Some(detail),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IvrStepTrace {
    pub call_id: String,
    pub session_id: String,
    pub caller: String,
    pub callee: String,
    pub step_index: u32,
    pub trigger: TriggerInfo,
    pub action_type: String,
    pub action_json: Option<String>,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub step_id: Option<String>,
    pub step_name: Option<String>,
    pub step_start_time: Option<String>,
    pub step_end_time: Option<String>,
    pub extra: Option<serde_json::Value>,
    pub sip_headers: Option<std::collections::HashMap<String, String>>,
    /// Filled only on the session-end trace entry that records how the whole
    /// IVR session ended. `None` for ordinary step entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<crate::call::app::ivr::provider::SessionEndTag>,
    /// Companion detail for [`end_reason`](Self::end_reason) (e.g. transfer target).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_detail: Option<String>,
    /// Identifier of the next node the flow advanced to (the successor's
    /// `step_id` in step mode). Carried once the successor is known; absent
    /// on terminal steps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_node_id: Option<String>,
    /// `step_id` of the next node — identical to [`Self::next_node_id`] in
    /// step mode; both are emitted so consumers can key on either name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step_id: Option<String>,
    /// Terminal marker: `true` on the flow's final observable step
    /// (terminal node, session end, or caller hangup mid-flow).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<bool>,
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
                end: None,
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
                end: true,
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

    #[test]
    fn transfer_source_serializes_nested_and_omits_none() {
        let payload = to_flat_payload(
            &CallTransferred {
                call_id: "call-1".into(),
                transfer_target: Some("queue:sales".into()),
                transfer_target_type: Some("queue".into()),
                transfer_source: Some(TransferSource {
                    source_type: "ivr".into(),
                    name: Some("main-ivr".into()),
                    ivr_node_id: Some("menu-2".into()),
                    agent_id: None,
                    agent_name: None,
                }),
            },
            None,
        );

        assert_eq!(payload["transfer_target_type"], "queue");
        assert_eq!(payload["transfer_source"]["source_type"], "ivr");
        assert_eq!(payload["transfer_source"]["name"], "main-ivr");
        assert_eq!(payload["transfer_source"]["ivr_node_id"], "menu-2");
        assert!(
            payload["transfer_source"].get("agent_id").is_none(),
            "None source fields must be omitted entirely"
        );
        assert!(
            payload["transfer_source"].get("agent_name").is_none(),
            "None agent_name must be omitted entirely"
        );
    }

    #[test]
    fn transfer_source_serializes_agent_id_and_name() {
        let payload = to_flat_payload(
            &CallTransferred {
                call_id: "call-1".into(),
                transfer_target: Some("sip:1002@rustpbx.com".into()),
                transfer_target_type: Some("sip".into()),
                transfer_source: Some(TransferSource {
                    source_type: "agent".into(),
                    name: None,
                    ivr_node_id: None,
                    agent_id: Some("1001".into()),
                    agent_name: Some("Alice".into()),
                }),
            },
            None,
        );

        assert_eq!(payload["transfer_source"]["source_type"], "agent");
        assert_eq!(payload["transfer_source"]["agent_id"], "1001");
        assert_eq!(payload["transfer_source"]["agent_name"], "Alice");
        assert!(
            payload["transfer_source"].get("name").is_none(),
            "agent branch keeps `name` unset (use agent_name)"
        );
    }

    #[test]
    fn call_transferred_omits_absent_source_fields() {
        let payload = to_flat_payload(
            &CallTransferred {
                call_id: "call-1".into(),
                transfer_target: Some("sip:1001@rustpbx.com".into()),
                transfer_target_type: Some("sip".into()),
                transfer_source: None,
            },
            None,
        );

        assert_eq!(payload["transfer_target_type"], "sip");
        assert!(
            payload.get("transfer_source").is_none(),
            "missing transfer_source must be omitted entirely"
        );
    }

    /// `queue_left` wire compatibility: without skill groups the payload is
    /// byte-identical to the legacy shape (no `skill_groups` key); with them
    /// the array rides along. `queue_overflow_joined` carries the newly
    /// joined overflow group.
    #[test]
    fn queue_left_skill_groups_wire_compat() {
        let plain = to_flat_payload(
            &QueueLeft {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                reason: Some("abandoned".into()),
                skill_groups: None,
            },
            None,
        );
        assert_eq!(plain["reason"], "abandoned");
        assert!(
            plain.get("skill_groups").is_none(),
            "no skill groups → field omitted (legacy wire format)"
        );

        let grouped = to_flat_payload(
            &QueueLeft {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                reason: Some("abandoned".into()),
                skill_groups: Some(vec!["support".into(), "support_l2".into()]),
            },
            None,
        );
        assert_eq!(
            grouped["skill_groups"],
            serde_json::json!(["support", "support_l2"]),
            "terminal queue_left carries the FULL skill-group history"
        );

        let joined = to_flat_payload(
            &QueueOverflowJoined {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                skill_group: "support_l2".into(),
            },
            None,
        );
        assert_eq!(joined["queue_id"], "support");
        assert_eq!(joined["skill_group"], "support_l2");
    }

    #[test]
    fn queue_overflow_joined_event_type() {
        assert_eq!(QueueOverflowJoined::TYPE, "queue_overflow_joined");
        assert_eq!(QueueLeft::TYPE, "queue_left");
    }
}
