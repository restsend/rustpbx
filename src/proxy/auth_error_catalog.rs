//! Error catalog for authentication / guest / realm rejections on the call
//! path (`src/proxy/auth.rs`). These are emitted alongside the SIP challenge
//! or abort so the CDR / RWI stream explains why a call was refused.

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

const APP: &str = "auth";

pub const AUTH_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "auth.failed",
    message: "Authentication failed",
    sip_status: Some(401),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.auth.failed",
    remediation_key: None,
};

pub const AUTH_REALM_MISMATCH: CallErrInfo = CallErrInfo {
    app: APP,
    code: "auth.realm_mismatch",
    message: "Authenticated user realm mismatch",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.auth.realm_mismatch",
    remediation_key: None,
};

pub const GUEST_DENIED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "auth.guest_denied",
    message: "Unknown caller rejected while guest calls are disabled",
    sip_status: Some(401),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.auth.guest_denied",
    remediation_key: None,
};

pub const PAYMENT_REQUIRED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "auth.payment_required",
    message: "Account payment required",
    sip_status: Some(402),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.auth.payment_required",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    AUTH_FAILED,
    AUTH_REALM_MISMATCH,
    GUEST_DENIED,
    PAYMENT_REQUIRED,
];
