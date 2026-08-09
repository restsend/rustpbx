//! Unified Command Dispatch
//!
//! This module provides a unified entry point for dispatching commands to sessions
//! via the `CallCommand` pipeline (RWI/console payloads are adapted into commands).
//!
//! ## Usage
//!
//! ```rust,ignore
//! use crate::call::runtime::command_dispatch::dispatch_command;
//!
//! // Convert RWI payload to CallCommand and dispatch
//! let result = dispatch_command(
//!     &registry,
//!     session_id,
//!     rwi_payload,
//!     CommandSource::Rwi,
//! ).await;
//! ```

#[cfg(feature = "console")]
use crate::call::adapters::console_to_call_command;
use crate::call::adapters::rwi_to_call_command;
use crate::call::domain::CallCommand;
use crate::call::runtime::CommandResult;
#[cfg(feature = "console")]
use crate::console::handlers::call_control::CallCommandPayload;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::rwi::session::RwiCommandPayload;
use std::sync::Arc;

/// Dispatch an RWI command using the unified path
///
/// Converts RwiCommandPayload to CallCommand and dispatches to the session.
/// Media capability checks happen inside the session (`execute_command`),
/// which knows the real media profile.
pub fn dispatch_rwi_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: Option<&str>,
    payload: RwiCommandPayload,
) -> anyhow::Result<CommandResult> {
    let command = match rwi_to_call_command(payload, session_id) {
        Ok(cmd) => cmd,
        Err(e) => {
            // Some RWI commands are not convertible to CallCommand (session management, etc.)
            // Return a special result indicating the command should be handled by legacy path
            return Ok(CommandResult::failure(format!(
                "command not supported by unified path: {}",
                e
            )));
        }
    };

    dispatch_command(registry, session_id.unwrap_or_default(), command)
}

/// Dispatch a Console command using the unified path
#[cfg(feature = "console")]
pub fn dispatch_console_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
    payload: CallCommandPayload,
) -> anyhow::Result<CommandResult> {
    let command = console_to_call_command(payload, session_id)?;
    dispatch_command(registry, session_id, command)
}

/// Internal: Dispatch a CallCommand to a session
fn dispatch_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
    command: CallCommand,
) -> anyhow::Result<CommandResult> {
    let Some(handle) = registry.get_handle(session_id) else {
        return Ok(CommandResult::failure(format!(
            "session {} not found",
            session_id
        )));
    };

    // Send the command to the session's event loop
    match handle.send_command(command) {
        Ok(_) => Ok(CommandResult::success()),
        Err(e) => Ok(CommandResult::failure(format!("failed to dispatch: {}", e))),
    }
}
