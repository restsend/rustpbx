//! # Command Adapters
//!
//! This module provides adapters to convert external command formats
//! to the unified `CallCommand` type.
//!
//! ## Supported Adapters
//!
//! - `RwiAdapter`: Converts `RwiCommandPayload` to `CallCommand`
//! - `ConsoleAdapter`: Converts `CallCommandPayload` (HTTP API) to `CallCommand`

use crate::callrecord::CallRecordHangupReason;

#[cfg(feature = "console")]
mod console_adapter;
mod rwi_adapter;

#[cfg(feature = "console")]
pub use console_adapter::*;
pub use rwi_adapter::*;

/// Common error type for adapter conversions
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("Missing required field: {0}")]
    MissingField(&'static str),

    #[error("Command not supported in current context: {0}")]
    NotSupported(String),
}

/// Convert a hangup reason string to a `CallRecordHangupReason`.
/// Shared by the RWI and Console adapters.
pub(crate) fn parse_hangup_reason(reason: Option<&str>) -> Option<CallRecordHangupReason> {
    reason.and_then(|r| match r.to_lowercase().as_str() {
        "by_caller" | "caller" => Some(CallRecordHangupReason::ByCaller),
        "by_callee" | "callee" => Some(CallRecordHangupReason::ByCallee),
        "by_system" | "system" => Some(CallRecordHangupReason::BySystem),
        "no_answer" => Some(CallRecordHangupReason::NoAnswer),
        "rejected" => Some(CallRecordHangupReason::Rejected),
        "canceled" => Some(CallRecordHangupReason::Canceled),
        "failed" => Some(CallRecordHangupReason::Failed),
        "abandoned" => Some(CallRecordHangupReason::Abandoned),
        _ => None,
    })
}
