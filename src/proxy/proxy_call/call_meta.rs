use crate::call::domain::{ReturnAppSpec, RtpTimeoutSide, TransferOutcome};
use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::state::SessionHangupMessage;
use rsipstack::dialog::DialogId;
use rsipstack::sip::StatusCode;
use std::collections::HashSet;
use std::time::Instant;

#[derive(Default)]
pub struct CallMeta {
    /// Root session id for the whole logical call: the first INVITE's
    /// Call-ID for inbound calls (or generated root for originates),
    /// inherited unchanged by every child leg (queue dispatch, REFER
    /// transfer, consultation). `None` means "this session is the root"
    /// — callers resolve with the session's own id. Carried across the
    /// network boundary via the RFC 7433 `User-to-User` header
    /// (`purpose=call-center`, see `call::uui`).
    pub root_session_id: Option<String>,
    pub connected_callee: Option<String>,
    pub connected_callee_dialog_id: Option<DialogId>,
    pub callee_call_ids: HashSet<String>,
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<SessionHangupMessage>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    /// The SIP status of the INVITE transaction's final response, captured once
    /// when call setup finalizes. Later signaling (BYE, transfer failures,
    /// re-INVITE, ...) must never change it — the CDR/CallEnded status is locked
    /// to this value.
    pub invite_final_status: Option<u16>,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub queue_name: Option<String>,
    /// Primary skill-group id when the queue dials a `skill-group:{id}` target.
    /// Post-call hooks (CSAT, wrapup, hold-music) resolve skill-group
    /// configuration via this id — not via the queue name label.
    pub skill_group_id: Option<String>,
    /// Standardized error code (from the [`crate::call_errors`] registry) for
    /// the last in-call failure. Mirrors `last_error` but carries a stable,
    /// queryable code; injected into call-record metadata by `record_snapshot`.
    pub error_code: Option<&'static crate::call_errors::CallErrInfo>,
    /// Name of the application flow currently driving the session
    /// (e.g. `ivr`, `voicemail`, `conference`, `queue`). Captured at flow start
    /// so failures are attributable to the running app in the call record.
    pub app_name: Option<String>,
    /// Optional display label of the queue driving the session (distinct from
    /// `queue_name` which is the machine identifier).
    pub queue_label: Option<String>,
    /// When set and the connected B‑leg (agent / bridge) terminates, the
    /// session returns the caller to this app instead of hanging up.
    /// Set by `handle_blind_transfer` for `TransferTarget::Sip` /
    /// `TransferTarget::Bridge` and `handle_queue_transfer` when the user
    /// configures `return_app` on the action.
    /// Consumed once by the `CallCommand::StartReturnApp` handler so the
    /// return is one-shot.
    pub transfer_return_app: Option<ReturnAppSpec>,
    pub pending_transfer_outcome: Option<TransferOutcome>,
    /// Ordered diagnostic timeline of the call (ring → answer → ivr → queue →
    /// transfer → bridge → hold/resume → plays → hangup). Persisted into the
    /// call-record `metadata["trace"]` array by `record_snapshot`.
    pub trace: Vec<crate::call_errors::TraceEvent>,
    /// Which side of the bridge fired the RTP-inactivity watchdog (if it did).
    /// Set from the `HangupCommand.rtp_timeout_side` in `handle_hangup`.
    pub rtp_timeout_side: Option<RtpTimeoutSide>,
    /// Human-readable label of the leg that went silent (display name +
    /// endpoint, or app name when an app drives the call). Persisted into the
    /// call-record `metadata["rtpTimeoutLeg"]`.
    pub rtp_timeout_leg: Option<String>,
    /// Whether the RTP-inactivity watchdog fired at any point, even if a
    /// higher-level reason (IVR end reason, caller hangup) eventually won the
    /// CDR `hangup_reason`. Lets `resolve_final_hangup_reason` preserve the
    /// RTP timeout trace without masking it.
    pub rtp_timeout_fired: bool,
    /// True while a blind transfer is in progress (new B-leg ringing / REFER
    /// awaiting completion). The RTP watchdog is suppressed during this window.
    pub transfer_in_progress: bool,
    /// Whether a real callee/agent leg ever answered (media path established
    /// to a remote party), even if that leg has since terminated. Never reset.
    /// Lets the queue-abandon detector distinguish "caller hung up while still
    /// waiting for an agent" from "caller hung up after already being served".
    pub ever_connected_callee: bool,
    /// One-shot: the fast-path relay-arm-failure monitor has been spawned.
    /// Both the app-answer and callee-answer paths would otherwise spawn a
    /// monitor on the same bridge latch, doubling the warn + the
    /// `RelayArmFailure` command (and the transcode fallback it triggers).
    pub relay_arm_monitor_spawned: bool,
    /// One-shot: the `RelayArmFailure` command was already handled (bridge
    /// forced into transcode mode). Duplicate commands are ignored.
    pub relay_arm_failure_handled: bool,
}

/// Queue name for this session (authoritative store in [`CallMeta`]).
pub fn effective_queue_name(meta: &CallMeta) -> Option<String> {
    meta.queue_name
        .clone()
        .or_else(|| meta.queue_label.clone())
        .filter(|s| !s.is_empty())
}

/// Skill-group id for this session (authoritative store in [`CallMeta`]).
pub fn effective_skill_group_id(meta: &CallMeta) -> Option<String> {
    meta.skill_group_id.clone()
}

pub fn has_queue_name(meta: &CallMeta) -> bool {
    effective_queue_name(meta).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_queue_name_falls_back_to_label() {
        let mut meta = CallMeta::default();
        meta.queue_label = Some("Sales Hotline".to_string());
        assert_eq!(
            effective_queue_name(&meta).as_deref(),
            Some("Sales Hotline")
        );
    }
}
