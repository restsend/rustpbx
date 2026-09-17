//! Error catalog for the predictive-outbound dial subsystem (`src/outbound/`).

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

const APP: &str = "outbound";

pub const WEBHOOK_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "outbound.webhook_failed",
    message: "Outbound post-answer webhook failed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.outbound.webhook_failed",
    remediation_key: Some("errors.outbound.webhook_failed.remedy"),
};

pub const CATALOG: &[CallErrInfo] = &[WEBHOOK_FAILED];
