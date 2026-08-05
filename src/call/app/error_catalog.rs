//! Error catalog for application-layer failures (`src/call/app/`): IVR,
//! voicemail and conference start/execute errors.

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

pub const IVR_START_FAILED: CallErrInfo = CallErrInfo {
    app: "ivr",
    code: "ivr.start_failed",
    message: "Failed to start IVR",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.ivr.start_failed",
    remediation_key: Some("errors.ivr.start_failed.remedy"),
};

pub const IVR_EXECUTE_ERROR: CallErrInfo = CallErrInfo {
    app: "ivr",
    code: "ivr.execute_error",
    message: "IVR execution error",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.ivr.execute_error",
    remediation_key: Some("errors.ivr.execute_error.remedy"),
};

pub const IVR_NORMAL: CallErrInfo = CallErrInfo {
    app: "ivr",
    code: "ivr.normal",
    message: "IVR flow completed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::BySystem,
    severity: ErrSeverity::Info,
    locale_key: "errors.ivr.normal",
    remediation_key: None,
};

pub const IVR_HANGUP: CallErrInfo = CallErrInfo {
    app: "ivr",
    code: "ivr.hangup",
    message: "IVR hung up",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::BySystem,
    severity: ErrSeverity::Info,
    locale_key: "errors.ivr.hangup",
    remediation_key: None,
};

pub const IVR_USER_HANGUP: CallErrInfo = CallErrInfo {
    app: "ivr",
    code: "ivr.user_hangup",
    message: "Caller hung up during IVR",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::ByCaller,
    severity: ErrSeverity::Info,
    locale_key: "errors.ivr.user_hangup",
    remediation_key: None,
};

pub const IVR_TIMEOUT: CallErrInfo = CallErrInfo {
    app: "ivr",
    code: "ivr.timeout",
    message: "IVR timeout",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Autohangup,
    severity: ErrSeverity::Warn,
    locale_key: "errors.ivr.timeout",
    remediation_key: Some("errors.ivr.timeout.remedy"),
};

pub const VOICEMAIL_START_FAILED: CallErrInfo = CallErrInfo {
    app: "voicemail",
    code: "voicemail.start_failed",
    message: "Failed to start voicemail",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.voicemail.start_failed",
    remediation_key: None,
};

pub const CONFERENCE_START_FAILED: CallErrInfo = CallErrInfo {
    app: "conference",
    code: "conference.start_failed",
    message: "Failed to start conference",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::ServerUnavailable,
    severity: ErrSeverity::Error,
    locale_key: "errors.conference.start_failed",
    remediation_key: None,
};

pub const APP_RUNTIME_ERROR: CallErrInfo = CallErrInfo {
    app: "app",
    code: "app.runtime_error",
    message: "Application runtime error",
    sip_status: Some(500),
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.app.runtime_error",
    remediation_key: None,
};

pub const CATALOG: &[CallErrInfo] = &[
    IVR_START_FAILED,
    IVR_EXECUTE_ERROR,
    IVR_NORMAL,
    IVR_HANGUP,
    IVR_USER_HANGUP,
    IVR_TIMEOUT,
    VOICEMAIL_START_FAILED,
    CONFERENCE_START_FAILED,
    APP_RUNTIME_ERROR,
];
