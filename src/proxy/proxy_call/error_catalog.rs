//! Error catalog for in-call failure sites: queue, dial/fork and transfer.
//! Owned by `src/proxy/proxy_call/` (sip_session + submodules).

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

// --- queue -----------------------------------------------------------------

pub const QUEUE_ABANDONED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.abandoned",
    message: "Caller abandoned the queue",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Abandoned,
    severity: ErrSeverity::Warn,
    locale_key: "errors.queue.abandoned",
    remediation_key: None,
};

pub const QUEUE_ALL_AGENTS_UNAVAILABLE: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.all_agents_unavailable",
    message: "All agents unavailable",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.queue.all_agents_unavailable",
    remediation_key: Some("errors.queue.all_agents_unavailable.remedy"),
};

pub const QUEUE_NO_AGENTS: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.no_agents",
    message: "No agents available",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.queue.no_agents",
    remediation_key: Some("errors.queue.no_agents.remedy"),
};

pub const QUEUE_NO_AGENTS_SKILL: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.no_agents_skill",
    message: "No agents available for skill group",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.queue.no_agents_skill",
    remediation_key: None,
};

pub const QUEUE_AGENT_REGISTRY_MISSING: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.agent_registry_missing",
    message: "Agent registry not available",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.agent_registry_missing",
    remediation_key: None,
};

pub const QUEUE_IVR_START_FAILED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.ivr_start_failed",
    message: "Failed to start IVR fallback",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.ivr_start_failed",
    remediation_key: None,
};

pub const QUEUE_VOICEMAIL_START_FAILED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.voicemail_start_failed",
    message: "Failed to start voicemail fallback",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.voicemail_start_failed",
    remediation_key: None,
};

pub const QUEUE_CONFERENCE_START_FAILED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.conference_start_failed",
    message: "Failed to start conference fallback",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.conference_start_failed",
    remediation_key: None,
};

pub const QUEUE_REDIRECT_FAILED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.redirect_failed",
    message: "Redirect failed",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.redirect_failed",
    remediation_key: None,
};

pub const QUEUE_REENQUEUE_FAILED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.reenqueue_failed",
    message: "Re-enqueue failed",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.reenqueue_failed",
    remediation_key: None,
};

pub const QUEUE_ALL_AGENTS_UNAVAILABLE_DEFAULT: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.all_agents_unavailable_default",
    message: "All agents unavailable",
    sip_status: Some(486),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.queue.all_agents_unavailable_default",
    remediation_key: None,
};

pub const QUEUE_TRANSFER_FAILED: CallErrInfo = CallErrInfo {
    app: "queue",
    code: "queue.transfer_failed",
    message: "Queue transfer failed",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.queue.transfer_failed",
    remediation_key: None,
};

// --- dial / fork -----------------------------------------------------------

pub const DIAL_ALL_TARGETS_FAILED: CallErrInfo = CallErrInfo {
    app: "dial",
    code: "dial.all_targets_failed",
    message: "All targets failed",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.dial.all_targets_failed",
    remediation_key: None,
};

pub const DIAL_CALLER_CANCELLED: CallErrInfo = CallErrInfo {
    app: "dial",
    code: "dial.caller_cancelled",
    message: "Caller cancelled",
    sip_status: Some(487),
    hangup_reason: CallRecordHangupReason::Canceled,
    severity: ErrSeverity::Info,
    locale_key: "errors.dial.caller_cancelled",
    remediation_key: None,
};

pub const DIAL_FORK_FAILED: CallErrInfo = CallErrInfo {
    app: "dial",
    code: "dial.fork_failed",
    message: "Target fork failed",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.dial.fork_failed",
    remediation_key: None,
};

pub const DIAL_FORK_JOIN_ERROR: CallErrInfo = CallErrInfo {
    app: "dial",
    code: "dial.fork_join_error",
    message: "Fork join error",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.dial.fork_join_error",
    remediation_key: None,
};

pub const DIAL_NO_TARGETS: CallErrInfo = CallErrInfo {
    app: "dial",
    code: "dial.no_targets",
    message: "No targets to dial",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.dial.no_targets",
    remediation_key: None,
};

pub const DIAL_NO_CALLER: CallErrInfo = CallErrInfo {
    app: "dial",
    code: "dial.no_caller",
    message: "No caller in dialplan",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.dial.no_caller",
    remediation_key: None,
};

// --- transfer --------------------------------------------------------------

pub const TRANSFER_REFER_REJECTED: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.refer_rejected",
    message: "REFER rejected by remote party",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Warn,
    locale_key: "errors.transfer.refer_rejected",
    remediation_key: None,
};

pub const TRANSFER_TIMEOUT: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.timeout",
    message: "Transfer timed out",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.transfer.timeout",
    remediation_key: None,
};

pub const TRANSFER_CANCELLED: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.cancelled",
    message: "Transfer cancelled",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Canceled,
    severity: ErrSeverity::Info,
    locale_key: "errors.transfer.cancelled",
    remediation_key: None,
};

pub const TRANSFER_INVALID_TARGET: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.invalid_target",
    message: "Invalid transfer target",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.transfer.invalid_target",
    remediation_key: None,
};

pub const TRANSFER_INVALID_STATE: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.invalid_state",
    message: "Transfer requested in invalid state",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.transfer.invalid_state",
    remediation_key: None,
};

pub const TRANSFER_INTERNAL_ERROR: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.internal_error",
    message: "Transfer internal error",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.transfer.internal_error",
    remediation_key: None,
};

pub const RTP_TIMEOUT: CallErrInfo = CallErrInfo {
    app: "proxy",
    code: "proxy.rtp_timeout",
    message: "RTP inactivity timeout",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::RtpTimeout,
    severity: ErrSeverity::Warn,
    locale_key: "errors.proxy.rtp_timeout",
    remediation_key: None,
};

/// Answered call where one or more bridge legs never delivered a single media
/// packet (transport RX == 0): browser ICE/DTLS never completed, one-way
/// NAT/UDP filtering, a muted/micless softphone, or a carrier answering
/// without media. Detected at call end — the call itself may have completed
/// normally (200 + BYE), so this never changes the hangup outcome; it only
/// annotates the CDR (error chip + trace) and logs one warn, so recordings
/// without audio are explainable. The affected side(s) ride
/// `metadata.mediaIssueLegs` ("caller" | "callee" | "caller+callee") and
/// `metadata.error_detail` (human-readable), and the callee side is only
/// checked for calls where a real remote callee leg answered (gated by
/// `CallMeta::ever_connected_callee`) so IVR/app playback legs are never
/// flagged.
pub const LEG_MEDIA_INCOMPLETE: CallErrInfo = CallErrInfo {
    app: "proxy",
    code: "proxy.leg_media_incomplete",
    message: "Answered but one or more legs delivered no media",
    sip_status: None,
    // Detection-only diagnostic: the call ended by its own cause (caller /
    // callee / system), never by this code — keep the outcome empty.
    hangup_reason: CallRecordHangupReason::Other(String::new()),
    severity: ErrSeverity::Warn,
    locale_key: "errors.proxy.leg_media_incomplete",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    QUEUE_ABANDONED,
    QUEUE_ALL_AGENTS_UNAVAILABLE,
    QUEUE_NO_AGENTS,
    QUEUE_NO_AGENTS_SKILL,
    QUEUE_AGENT_REGISTRY_MISSING,
    QUEUE_IVR_START_FAILED,
    QUEUE_VOICEMAIL_START_FAILED,
    QUEUE_CONFERENCE_START_FAILED,
    QUEUE_REDIRECT_FAILED,
    QUEUE_REENQUEUE_FAILED,
    QUEUE_ALL_AGENTS_UNAVAILABLE_DEFAULT,
    QUEUE_TRANSFER_FAILED,
    DIAL_ALL_TARGETS_FAILED,
    DIAL_CALLER_CANCELLED,
    DIAL_FORK_FAILED,
    DIAL_FORK_JOIN_ERROR,
    DIAL_NO_TARGETS,
    DIAL_NO_CALLER,
    TRANSFER_REFER_REJECTED,
    TRANSFER_TIMEOUT,
    TRANSFER_CANCELLED,
    TRANSFER_INVALID_TARGET,
    TRANSFER_INVALID_STATE,
    TRANSFER_INTERNAL_ERROR,
    RTP_TIMEOUT,
    LEG_MEDIA_INCOMPLETE,
];
