//! Error catalog for the HTTP router (`src/proxy/routing/http.rs`).

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

const APP: &str = "http_router";

pub const UPSTREAM_ERROR: CallErrInfo = CallErrInfo {
    app: APP,
    code: "http_router.upstream_error",
    message: "HTTP router returned an error",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.http_router.upstream_error",
    remediation_key: Some("errors.http_router.upstream_error.remedy"),
};

pub const UPSTREAM_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "http_router.upstream_failed",
    message: "HTTP router request failed",
    sip_status: Some(503),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.http_router.upstream_failed",
    remediation_key: Some("errors.http_router.upstream_failed.remedy"),
};

pub const PARSE_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "http_router.parse_failed",
    message: "Failed to parse HTTP router response",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.http_router.parse_failed",
    remediation_key: None,
};

pub const SPAM: CallErrInfo = CallErrInfo {
    app: APP,
    code: "http_router.spam",
    message: "Marked as spam by HTTP router",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.http_router.spam",
    remediation_key: None,
};

pub const REJECTED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "http_router.rejected",
    message: "Rejected by HTTP router",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.http_router.rejected",
    remediation_key: None,
};

pub const NOT_HANDLED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "http_router.not_handled",
    message: "Not handled by HTTP router",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Info,
    locale_key: "errors.http_router.not_handled",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    UPSTREAM_ERROR,
    UPSTREAM_FAILED,
    PARSE_FAILED,
    SPAM,
    REJECTED,
    NOT_HANDLED,
];
