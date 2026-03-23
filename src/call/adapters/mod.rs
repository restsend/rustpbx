//! # Command Adapters
//!
//! This module provides adapters to convert external command formats
//! to the unified `CallCommand` type.
//!
//! ## Supported Adapters
//!
//! - `RwiAdapter`: Converts `RwiCommandPayload` to `CallCommand`
//! - `ConsoleAdapter`: Converts `CallCommandPayload` (HTTP API) to `CallCommand`
//! - `SessionActionBridge`: Converts `CallCommand` to `SessionAction` (migration only)

mod console_adapter;
mod rwi_adapter;
mod session_action_bridge;

pub use console_adapter::*;
pub use rwi_adapter::*;
pub use session_action_bridge::*;

/// Common error type for adapter conversions
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("Unknown command type")]
    UnknownCommand,

    #[error("Missing required field: {0}")]
    MissingField(&'static str),

    #[error("Invalid value for field '{0}': {1}")]
    InvalidValue(&'static str, String),

    #[error("Command not supported in current context: {0}")]
    NotSupported(String),

    #[error("Session command without session context")]
    NoSessionContext,
}
