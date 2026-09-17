//! Error catalog for reachability lookups (`locator`, registrar, in-call
//! target resolution). Failures here mean a callee could not be located, so a
//! call may be rejected, routed to an offline/wrong destination, or dialed
//! bare — all of which now surface through the unified `call_error` pipeline.

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

const APP: &str = "locator";

pub const LOOKUP_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "locator.lookup_failed",
    message: "Contact locator lookup failed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.locator.lookup_failed",
    remediation_key: Some("errors.locator.lookup_failed.remedy"),
};

pub const BACKEND_ERROR: CallErrInfo = CallErrInfo {
    app: APP,
    code: "locator.backend_error",
    message: "Contact locator backend error",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.locator.backend_error",
    remediation_key: None,
};

pub const WEBHOOK_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "locator.webhook_failed",
    message: "Contact locator webhook delivery failed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.locator.webhook_failed",
    remediation_key: None,
};

pub const TARGET_UNREACHABLE: CallErrInfo = CallErrInfo {
    app: APP,
    code: "locator.target_unreachable",
    message: "Dial target not registered / not locatable",
    sip_status: Some(480),
    hangup_reason: CallRecordHangupReason::NoAnswer,
    severity: ErrSeverity::Warn,
    locale_key: "errors.locator.target_unreachable",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    LOOKUP_FAILED,
    BACKEND_ERROR,
    WEBHOOK_FAILED,
    TARGET_UNREACHABLE,
];
