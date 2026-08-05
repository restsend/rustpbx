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

pub const TRANSFER_THREE_PCC_FAILED: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.three_pcc_failed",
    message: "3PCC transfer failed",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.transfer.three_pcc_failed",
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

pub const TRANSFER_BRIDGE_FAILED: CallErrInfo = CallErrInfo {
    app: "transfer",
    code: "transfer.bridge_failed",
    message: "Transfer bridge failed",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.transfer.bridge_failed",
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
    TRANSFER_THREE_PCC_FAILED,
    TRANSFER_TIMEOUT,
    TRANSFER_CANCELLED,
    TRANSFER_INVALID_TARGET,
    TRANSFER_INVALID_STATE,
    TRANSFER_BRIDGE_FAILED,
    TRANSFER_INTERNAL_ERROR,
];
