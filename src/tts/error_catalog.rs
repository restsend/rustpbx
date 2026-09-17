//! Error catalog for the TTS subsystem (`src/tts/`).
//!
//! TTS failures are call-affecting when a prompt cannot be synthesized: the
//! IVR/voicemail flow either falls back to a static file, falls back to the
//! local `edge-cli`, or degrades to silence. Each entry is reported through the
//! unified `call_error` pipeline (`stage = "tts"`).

use crate::call_errors::{CallErrInfo, ErrSeverity};
use crate::callrecord::CallRecordHangupReason;

const APP: &str = "tts";

pub const SYNTHESIS_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "tts.synthesis_failed",
    message: "TTS synthesis failed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.tts.synthesis_failed",
    remediation_key: Some("errors.tts.synthesis_failed.remedy"),
};

pub const HTTP_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "tts.http_failed",
    message: "TTS HTTP driver request failed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.tts.http_failed",
    remediation_key: None,
};

pub const CLI_FAILED: CallErrInfo = CallErrInfo {
    app: APP,
    code: "tts.cli_failed",
    message: "TTS CLI driver request failed",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Error,
    locale_key: "errors.tts.cli_failed",
    remediation_key: None,
};

pub const NO_SERVICE: CallErrInfo = CallErrInfo {
    app: APP,
    code: "tts.no_service",
    message: "No TTS service configured; using local fallback",
    sip_status: None,
    hangup_reason: CallRecordHangupReason::Failed,
    severity: ErrSeverity::Warn,
    locale_key: "errors.tts.no_service",
    remediation_key: Some("errors.tts.no_service.remedy"),
};

pub const CATALOG: &[CallErrInfo] = &[SYNTHESIS_FAILED, HTTP_FAILED, CLI_FAILED, NO_SERVICE];
