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
    /// The leg the call was created for: the caller leg of an inbound session
    /// (`"caller"`), the outgoing INVITE leg of an API originate (`leg_id`
    /// `None`, `leg_role` `"caller"` — no leg registry entry exists
    /// pre-answer). Child-leg sessions (a cluster peer dispatched a queue
    /// agent / consult / transfer leg to this node) emit no `call_created`
    /// at all — the dispatching node already announced the logical call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// `caller` | `callee` | `agent` | `consult` | `transfer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub call_id: String,
    pub context: String,
    pub caller: String,
    pub callee: String,
    pub trunk: Option<String>,
    /// Every SIP header of the request that created the call, minus the
    /// transport/dialog headers with typed representations (Via, Call-ID,
    /// CSeq, Content-Length/Type, From, To, User-Agent, Allow, Max-Forwards).
    /// Route-injected headers replace same-name entries (case-insensitive).
    /// Populated on every production emission point (inbound INVITE, RWI app,
    /// API originate — extracted from the actual outgoing INVITE).
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
    /// Per-leg events fire for every registered leg's ring transition
    /// (queue-dispatched agent, consult target, …) AND for the originate
    /// A dialog: the outgoing INVITE leg (`"caller"`) emits its own ringing
    /// on ring-state change (first provisional, then the upgrade to early
    /// media), so an API/CTI outbound carries the same leg-level contract as
    /// inbound flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// `agent` | `consult` | `transfer` — present on leg events and on the
    /// session events of a child-leg session (a cluster peer dispatched the
    /// leg to this node). `agent` identifies the CC agent leg (queue
    /// dispatch, consult to a registered agent, agent-initiated call).
    ///
    /// Multiple `call_ringing` per call are the per-leg contract: every
    /// agent attempt (sequential re-dial, escalation stage, parallel fork)
    /// rings its own leg. After a blind/REFER transfer the departing agent's
    /// attribution is cleared — the replacement target's ringing does not
    /// inherit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub call_id: String,
    /// `true` when the ringing provisional response carried SDP (183 Session
    /// Progress or 180 with SDP — early media), `false` for a plain 180
    /// Ringing. Emitted once per provisional response; consumers tell
    /// ringback from early media by this flag instead of a separate event.
    #[serde(default)]
    pub early_media: bool,
    /// Agent attribution pinned on the ringing LEG event before the INVITE;
    /// session-level events leave it `None` (call-meta enrichment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Display name, only when it unambiguously belongs to `agent_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}
rwi_event!(CallRinging, "call_ringing");

/// A call leg ANSWERED. Emitted per leg — every leg answer (caller pickup,
/// direct-answer callee, queue-dispatched agent, consult / transfer target)
/// carries its own `leg_id` + `leg_role`.
///
/// A session-scoped duplicate (`leg_id: None`) is emitted at most once per
/// call at the agent / remote connect moment. **Deprecated**: it will be
/// removed in the next version — consume the leg-level events and drop the
/// session-scoped one. Child-leg sessions (a cluster peer dispatched the leg
/// to this node) never emit the session-scoped duplicate; their leg-level
/// event carries the leg.
#[derive(Debug, Clone, Serialize)]
pub struct CallAnswered {
    /// The leg that answered — emitted for EVERY leg answer: the caller
    /// pickup under a queue/IVR app (`"caller"`), a direct-answer callee,
    /// a queue-dispatched agent leg (`"agent"`), consult / transfer targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// `agent` | `consult` | `transfer` | `caller` | `callee` — the leg's own
    /// role, same semantics as `call_ringing` / `call_hangup` leg events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub call_id: String,
}
rwi_event!(CallAnswered, "call_answered");

/// The lifecycle owner whose termination a hangup reports.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallHangupScope {
    /// The root session ends the logical call.
    Call,
    /// A leg or a cluster-dispatched child session ends.
    Leg,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallHangup {
    /// Explicit lifecycle scope; independent of optional leg attribution.
    pub scope: CallHangupScope,
    /// Present for an individual leg event; absent for a session event.
    ///
    /// Leg-scoped events fire for:
    /// * a mid-session leg exit the session survives (setup failure, BYE on
    ///   a dial leg, REFER / conference detach),
    /// * an in-setup leg CANCELLED by the app — queue ring timeout
    ///   (`reason: "ring_timeout"`), escalation stage switch
    ///   (`"stage_switch"`), superseded parallel dial (`"superseded"`),
    ///   other explicit removals (`"cancelled"`).
    /// * CC-role legs (agent / consult / transfer) still CONNECTED when the
    ///   session tears down — emitted right before the session-scoped event
    ///   with identical `reason` / `hangup_by` / `duration_secs`, so
    ///   `leg_role == "agent"` consumers see the agent leg's teardown even
    ///   when the OTHER side hung up first (e.g. a CTI click-to-dial where
    ///   the customer ends the call). Plain caller/callee legs stay covered
    ///   by the session-scoped event alone.
    /// Miss hangups are what agent workbenches clear the ringing UI and
    /// analytics count unanswered calls against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// `agent` | `consult` | `transfer` — present on leg events and on the
    /// session events of a child-leg session (a cluster peer dispatched the
    /// leg to this node; the role is the intent the dispatcher declared).
    /// `agent` identifies the CC agent leg (queue dispatch, consult to a
    /// registered agent, agent-initiated call).
    ///
    /// Lifecycle contract: a leg-scoped `call_hangup` IS the leg's terminal
    /// transition — every emission site completes it by removing the leg, so
    /// a leg is reported exactly once. The session-scoped event of a
    /// child-leg session carries the declared role only while that leg is
    /// still registered (its teardown has not been reported yet); once the
    /// leg exited through its own terminal transition, the session event
    /// carries no role. After a blind/REFER transfer the departing agent's
    /// attribution is cleared — the call's next stage (IVR / extension /
    /// another agent) does not inherit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub call_id: String,
    pub reason: Option<String>,
    /// Normalized initiator: `"agent"` | `"caller"` | `"system"` | `"transfer"`
    /// | `"unknown"` (plus the `"callee"` fallback for non-CC calls).
    /// Consistent with the former CC-layer `cc_hangup.hangup_by`.
    ///
    /// Leg-scoped events attribute only established-dialog terminations: a
    /// remote BYE normalizes exactly like the session event (`"agent"` in a
    /// CC context, `"callee"` otherwise), a locally issued BYE reports
    /// `"system"`. Setup failures (never-connected dials) carry `None` —
    /// `queue_agent_no_answer` / `queue_agent_rejected` already describe
    /// those with `leg_role: "agent"`.
    ///
    /// Direction-aware correction: on an OUTBOUND call whose first terminal
    /// BYE came from the caller/A dialog's remote party, the initiator is
    /// the dialed CUSTOMER (`"callee"`), never `"agent"` — the CC-centric
    /// callee→agent mapping only applies when the BYE came from a
    /// callee/agent dialog or an inbound session.
    pub hangup_by: Option<String>,
    pub sip_status: Option<u16>,
    /// Talk time in seconds (answer → hangup). `None` when the call was never
    /// answered (originate setup failures). Leg-scoped events carry it only
    /// for a BYE-terminated established dialog; session-scoped events always
    /// carry it once the session ended.
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
    /// `agent` | `consult` | `caller` | `callee`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub call_id: String,
    pub leg_id: String,
}
rwi_event!(CallHeld, "call_held");

/// A previously held call leg was retrieved (resume). Replaces the former
/// `cc_unheld` event.
#[derive(Debug, Clone, Serialize)]
pub struct CallUnheld {
    /// `agent` | `consult` | `caller` | `callee`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
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

/// The originate target leg did not answer (ring timeout / 408 / 480 / 487)
/// or reported busy (486 / 600). The unanswered leg is the outgoing INVITE
/// leg — the session's `caller` leg: `leg_role` is `"caller"`, `leg_id` is
/// `None` (no leg registry entry exists pre-answer).
#[derive(Debug, Clone, Serialize)]
pub struct CallNoAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub call_id: String,
}
rwi_event!(CallNoAnswer, "call_no_answer");

/// The originate target leg reported busy (486 / 600). Same leg semantics as
/// [`CallNoAnswer`].
#[derive(Debug, Clone, Serialize)]
pub struct CallBusy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
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
    /// Role of the target leg (`caller`/`callee`/`agent`/…) — resolved via
    /// the session's leg table when the target leg is tracked there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub track_id: String,
}
rwi_event!(MediaPlayStarted, "media_play_started");

#[derive(Debug, Clone, Serialize)]
pub struct MediaPlayFinished {
    pub call_id: String,
    pub leg_id: Option<String>,
    /// Role of the target leg — resolved via the session's leg table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub track_id: String,
    pub interrupted: bool,
}
rwi_event!(MediaPlayFinished, "media_play_finished");

#[derive(Debug, Clone, Serialize)]
pub struct Dtmf {
    pub call_id: String,
    pub digit: String,
    pub leg_id: Option<String>,
    /// `agent` | `consult` | `caller` | `callee` — present when the source leg
    /// is tracked by the session (always, for session-originated DTMF).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
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
    /// Skill groups from the queue's dial targets at join time; `None` when
    /// not skill-group routed (mirrors `QueueLeft::skill_groups`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_groups: Option<Vec<String>>,
    /// Per-call ACD queue priority (higher preempts earlier waiters). `None`
    /// for the default priority 0 — absent and 0 are equivalent, so
    /// default-priority flows keep the exact pre-field wire payload. Consumed
    /// by the shared-queue event tap to persist `cc_acd_queue.priority`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
    /// Re-entry counter: absent on the call's FIRST join (wire-compatible
    /// with the pre-field payload), `2`, `3`, … on every re-entry after an
    /// agent attempt failed (no-answer / cancel / reject) and the call keeps
    /// queuing in the SAME queue — next candidate or another wait-retention
    /// round. A skill-GROUP switch is a different contract: it emits
    /// `queue_left{overflow}` + `queue_overflow_joined`, never a re-entry
    /// `queue_joined`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Explicit `null` on the wire: join events describe the CALL's entry,
    /// not an agent. Without this key the gateway's context enrichment would
    /// merge the session's current `agent_id` into RE-ENTRY joins — after a
    /// reject / no-answer round that attributes the rejoin to the FAILED
    /// agent (and races the failed leg's own teardown events the other way).
    /// `null` blocks the merge and keeps every join unattributed.
    pub agent_id: Option<String>,
}
rwi_event!(QueueJoined, "queue_joined");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentOffered {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
    /// Session leg id of the dial toward this agent — joins `queue_agent_*`
    /// with the leg-scoped `call_*` events (`call_ringing` / `call_hangup`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// Always `"agent"` — these events describe the CC agent leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
}
rwi_event!(QueueAgentOffered, "queue_agent_offered");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentConnected {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
    /// Session leg id that connected (see [`QueueAgentOffered::leg_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// Always `"agent"` — these events describe the CC agent leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
}
rwi_event!(QueueAgentConnected, "queue_agent_connected");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentNoAnswer {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
    pub attempt: u32,
    /// Session leg id of the failed dial (see [`QueueAgentOffered::leg_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// Always `"agent"` — these events describe the CC agent leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
}
rwi_event!(QueueAgentNoAnswer, "queue_agent_no_answer");

#[derive(Debug, Clone, Serialize)]
pub struct QueueAgentRejected {
    pub call_id: String,
    pub queue_id: String,
    pub agent_id: String,
    pub attempt: u32,
    /// Session leg id of the rejected dial (see [`QueueAgentOffered::leg_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_id: Option<String>,
    /// Always `"agent"` — these events describe the CC agent leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
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
    /// Per-call ACD queue priority carried through the move — the shared-
    /// queue tap re-registers the row on this event (delete + insert), so
    /// dropping it would reset the caller to the default tier. `None` ==
    /// default priority 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u32>,
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
    /// Role of the leg that delivered the digits ("caller" for IVR
    /// interactions). `None` when the role cannot be determined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
    pub digits: String,
}
rwi_event!(DtmfCollected, "dtmf_collected");

#[derive(Debug, Clone, Serialize)]
pub struct DtmfCollectionTimeout {
    pub call_id: String,
    pub leg_id: String,
    /// Role of the leg that was collecting (see [`DtmfCollected::leg_role`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
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
    /// Role of the joining leg ("caller" for room dial-in, "agent" /
    /// "consult" for bridged participants). `None` when undeterminable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
}
rwi_event!(ConferenceJoined, "conference_joined");

/// A call leg left a conference room (hangup or room teardown).
#[derive(Debug, Clone, Serialize)]
pub struct ConferenceLeft {
    pub conf_id: String,
    pub call_id: String,
    pub leg_id: String,
    /// Role of the leaving leg (see [`ConferenceJoined::leg_role`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg_role: Option<String>,
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

impl IvrStepTrace {
    /// Completion contract: every emitted entry carries BOTH stamps, so
    /// consumers can rely on them unconditionally — `step_end_time` is the
    /// completion marker, and `step_start_time` falls back to it (never
    /// later than the end). `duration_ms` is already non-optional by type.
    /// Idempotent: entries that already carry stamps pass through untouched.
    pub fn with_completion_stamps(mut self) -> Self {
        if self.step_end_time.is_none() {
            self.step_end_time = Some(chrono::Utc::now().to_rfc3339());
        }
        if self.step_start_time.is_none() {
            self.step_start_time = self.step_end_time.clone();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Completion contract on the RWI event type itself: an entry built
    /// without stamps is repaired (end = now, start = end) and an entry that
    /// already carries stamps passes through untouched (idempotent).
    #[test]
    fn ivr_step_trace_completion_stamps_backfilled_and_idempotent() {
        let mk = |start: Option<String>, end: Option<String>| IvrStepTrace {
            call_id: "call-1".into(),
            session_id: "sess-1".into(),
            caller: "1001".into(),
            callee: "2000".into(),
            step_index: 1,
            trigger: TriggerInfo::new("ivr_fallback"),
            action_type: "ivr_fallback".into(),
            action_json: None,
            duration_ms: 0,
            error: None,
            step_id: None,
            step_name: None,
            step_start_time: start,
            step_end_time: end,
            extra: None,
            sip_headers: None,
            end_reason: None,
            end_detail: None,
            next_node_id: None,
            next_step_id: None,
            end: None,
        };

        // Both missing → both backfilled, end >= start.
        let repaired = mk(None, None).with_completion_stamps();
        let start = repaired
            .step_start_time
            .as_deref()
            .expect("start backfilled");
        let end = repaired.step_end_time.as_deref().expect("end backfilled");
        assert!(
            chrono::DateTime::parse_from_rfc3339(end).unwrap()
                >= chrono::DateTime::parse_from_rfc3339(start).unwrap(),
            "backfilled end must not precede start"
        );

        // Start missing with an explicit end → start falls back to end.
        let end_only = mk(None, Some("2026-01-01T00:00:05+00:00".into())).with_completion_stamps();
        assert_eq!(
            end_only.step_start_time.as_deref(),
            Some("2026-01-01T00:00:05+00:00"),
            "start falls back to the explicit end"
        );

        // Already complete → untouched.
        let untouched = mk(
            Some("2026-01-01T00:00:00+00:00".into()),
            Some("2026-01-01T00:00:01+00:00".into()),
        )
        .with_completion_stamps();
        assert_eq!(
            untouched.step_start_time.as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
        assert_eq!(
            untouched.step_end_time.as_deref(),
            Some("2026-01-01T00:00:01+00:00")
        );
    }

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
                leg_role: None,
                extra: None,
            },
            None,
        );
        let dtmf_with_extra = to_flat_payload(
            &Dtmf {
                call_id: "call-2".into(),
                digit: "9".into(),
                leg_id: Some("caller".into()),
                leg_role: None,
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
                priority: None,
            },
            None,
        );
        assert_eq!(joined["queue_id"], "support");
        assert_eq!(joined["skill_group"], "support_l2");
        assert!(
            joined.get("priority").is_none(),
            "default priority must stay absent from the wire payload"
        );
        let joined_prio = to_flat_payload(
            &QueueOverflowJoined {
                call_id: "call-1".into(),
                queue_id: "support".into(),
                skill_group: "support_l2".into(),
                priority: Some(7),
            },
            None,
        );
        assert_eq!(
            joined_prio["priority"], 7,
            "non-default priority rides along"
        );
    }

    #[test]
    fn queue_overflow_joined_event_type() {
        assert_eq!(QueueOverflowJoined::TYPE, "queue_overflow_joined");
        assert_eq!(QueueLeft::TYPE, "queue_left");
    }
}
