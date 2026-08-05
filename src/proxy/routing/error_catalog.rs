//! Error catalog for the routing ACL / policy layer (`src/proxy/routing/matcher.rs`).
//!
//! Covers source-trunk and destination-trunk limit / policy rejections.

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

const APP: &str = "acl";

pub const SOURCE_CPS_LIMIT: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.source_cps_limit",
    message: "Source trunk CPS limit exceeded",
    sip_status: Some(503),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.source_cps_limit",
    remediation_key: Some("errors.acl.source_cps_limit.remedy"),
};

pub const SOURCE_CONCURRENT_LIMIT: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.source_concurrent_limit",
    message: "Source trunk concurrent call limit exceeded",
    sip_status: Some(503),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.source_concurrent_limit",
    remediation_key: Some("errors.acl.source_concurrent_limit.remedy"),
};

pub const POLICY_REJECTED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.policy_rejected",
    message: "Call rejected by policy rule",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.policy_rejected",
    remediation_key: None,
};

pub const REJECT_ACTION: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.reject_action",
    message: "Call rejected by reject action",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.reject_action",
    remediation_key: None,
};

pub const BUSY_ACTION: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.busy_action",
    message: "Call rejected as busy",
    sip_status: Some(486),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Info,
    locale_key: "errors.acl.busy_action",
    remediation_key: None,
};

pub const DEST_CPS_LIMIT: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.dest_cps_limit",
    message: "Destination trunk CPS limit exceeded",
    sip_status: Some(503),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.dest_cps_limit",
    remediation_key: Some("errors.acl.dest_cps_limit.remedy"),
};

pub const DEST_CONCURRENT_LIMIT: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.dest_concurrent_limit",
    message: "Destination trunk concurrent call limit exceeded",
    sip_status: Some(503),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.dest_concurrent_limit",
    remediation_key: Some("errors.acl.dest_concurrent_limit.remedy"),
};

pub const TRUNK_POLICY_REJECTED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "acl.trunk_policy_rejected",
    message: "Call rejected by trunk policy",
    sip_status: Some(403),
    hangup_reason: CallRecordHangupReason::Rejected,
    severity: ErrSeverity::Warn,
    locale_key: "errors.acl.trunk_policy_rejected",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    SOURCE_CPS_LIMIT,
    SOURCE_CONCURRENT_LIMIT,
    POLICY_REJECTED,
    REJECT_ACTION,
    BUSY_ACTION,
    DEST_CPS_LIMIT,
    DEST_CONCURRENT_LIMIT,
    TRUNK_POLICY_REJECTED,
];
