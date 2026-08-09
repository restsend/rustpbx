use crate::call::domain::{ReturnAppSpec, RtpTimeoutSide};
use crate::callrecord::CallRecordHangupReason;
use crate::proxy::proxy_call::state::SessionHangupMessage;
use rsipstack::dialog::DialogId;
use rsipstack::sip::StatusCode;
use std::collections::HashSet;
use std::time::Instant;

pub struct CallMeta {
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
}

impl CallMeta {
    pub fn new() -> Self {
        Self {
            connected_callee: None,
            connected_callee_dialog_id: None,
            callee_call_ids: HashSet::new(),
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            hangup_messages: Vec::new(),
            last_error: None,
            invite_final_status: None,
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            queue_name: None,
            error_code: None,
            app_name: None,
            queue_label: None,
            transfer_return_app: None,
            trace: Vec::new(),
            rtp_timeout_side: None,
            rtp_timeout_leg: None,
            rtp_timeout_fired: false,
            transfer_in_progress: false,
            ever_connected_callee: false,
        }
    }
}

impl Default for CallMeta {
    fn default() -> Self {
        Self::new()
    }
}
