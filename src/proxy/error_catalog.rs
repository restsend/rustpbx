//! Error catalog for the proxy / dialplan resolver (`src/proxy/call.rs`).
//!
//! These cover failures produced while building the dialplan: callee-offline,
//! realm rejections, always-forwarding misconfiguration and parse errors.

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

/// `app` prefix shared by every entry in this catalog.
const APP: &str = "proxy";

pub const CALLEE_OFFLINE: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.callee_offline",
    message: "Target user is offline",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.proxy.callee_offline",
    remediation_key: Some("errors.proxy.callee_offline.remedy"),
};

pub const EXTERNAL_REALM_BOTH: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.external_realm_both",
    message: "Both caller and callee are external realm",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.external_realm_both",
    remediation_key: Some("errors.proxy.external_realm_both.remedy"),
};

pub const CALLEE_URI_INVALID: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.callee_uri_invalid",
    message: "Invalid callee URI",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.callee_uri_invalid",
    remediation_key: None,
};

pub const MISSING_CALL_ID: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.missing_call_id",
    message: "Missing Call-ID header",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.missing_call_id",
    remediation_key: None,
};

pub const FROM_HEADER_PARSE: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.from_header_parse",
    message: "Failed to parse From header",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.from_header_parse",
    remediation_key: None,
};

pub const ALWAYS_FWD_URI_INVALID: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_uri_invalid",
    message: "Invalid always-forwarding target URI",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_uri_invalid",
    remediation_key: None,
};

pub const ALWAYS_FWD_QUEUE_EMPTY: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_queue_empty",
    message: "Always-forwarding queue reference is empty",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_queue_empty",
    remediation_key: None,
};

pub const ALWAYS_FWD_QUEUE_RESOLVE: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_queue_resolve",
    message: "Failed to resolve always-forwarding queue",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_queue_resolve",
    remediation_key: None,
};

pub const ALWAYS_FWD_QUEUE_NOT_FOUND: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_queue_not_found",
    message: "Always-forwarding queue not found",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.proxy.always_forwarding_queue_not_found",
    remediation_key: None,
};

pub const ALWAYS_FWD_QUEUE_BUILD: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_queue_build",
    message: "Failed to build always-forwarding queue plan",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_queue_build",
    remediation_key: None,
};

pub const ALWAYS_FWD_IVR_EMPTY: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_ivr_empty",
    message: "Always-forwarding IVR name is empty",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_ivr_empty",
    remediation_key: None,
};

pub const ALWAYS_FWD_VOICEMAIL_EMPTY: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_voicemail_empty",
    message: "Always-forwarding voicemail extension is empty",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_voicemail_empty",
    remediation_key: None,
};

pub const ALWAYS_FWD_CONFERENCE_EMPTY: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.always_forwarding_conference_empty",
    message: "Always-forwarding conference id is empty",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.always_forwarding_conference_empty",
    remediation_key: None,
};

pub const ROUTE_PREVIEW_ERROR: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.route_preview_error",
    message: "Route preview failed",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.route_preview_error",
    remediation_key: None,
};

pub const ROUTE_ABORTED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.route_aborted",
    message: "Route aborted during preview",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Warn,
    locale_key: "errors.proxy.route_aborted",
    remediation_key: None,
};

pub const CREATE_ROUTE_INVITE_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "proxy.create_route_invite_failed",
    message: "Failed to create route invite",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.proxy.create_route_invite_failed",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    CALLEE_OFFLINE,
    EXTERNAL_REALM_BOTH,
    CALLEE_URI_INVALID,
    MISSING_CALL_ID,
    FROM_HEADER_PARSE,
    ALWAYS_FWD_URI_INVALID,
    ALWAYS_FWD_QUEUE_EMPTY,
    ALWAYS_FWD_QUEUE_RESOLVE,
    ALWAYS_FWD_QUEUE_NOT_FOUND,
    ALWAYS_FWD_QUEUE_BUILD,
    ALWAYS_FWD_IVR_EMPTY,
    ALWAYS_FWD_VOICEMAIL_EMPTY,
    ALWAYS_FWD_CONFERENCE_EMPTY,
    ROUTE_PREVIEW_ERROR,
    ROUTE_ABORTED,
    CREATE_ROUTE_INVITE_FAILED,
];
