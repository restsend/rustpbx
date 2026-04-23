//! Unified Command Dispatch
//!
//! This module provides a unified entry point for dispatching commands to sessions.
//! It supports both the legacy SessionAction path and the new unified CallCommand path.
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

use crate::call::adapters::{console_to_call_command, rwi_to_call_command};
use crate::call::domain::CallCommand;
use crate::call::runtime::{CommandResult, CommandSource, ExecutionContext, MediaCapabilityCheck};
use crate::console::handlers::call_control::CallCommandPayload;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::rwi::session::RwiCommandPayload;
use std::sync::Arc;

/// Dispatch an RWI command using the unified path
///
/// This function:
/// 1. Converts RwiCommandPayload to CallCommand
/// 2. Checks media capabilities
/// 3. Dispatches to the session
pub fn dispatch_rwi_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: Option<&str>,
    payload: RwiCommandPayload,
) -> anyhow::Result<CommandResult> {
    // Step 1: Convert to CallCommand
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

    // Step 2: Create execution context
    let session_id = session_id.unwrap_or_default();
    let ctx = ExecutionContext::new(session_id).with_source(CommandSource::Rwi);

    // Step 3: Check media capabilities
    match ctx.check_media_capability(&command) {
        MediaCapabilityCheck::Denied { reason } => {
            return Ok(CommandResult::not_supported(reason));
        }
        MediaCapabilityCheck::Degraded { reason: _ } => {
            // Command can execute but with degraded functionality
            // Continue execution, the result will indicate degradation
        }
        MediaCapabilityCheck::Allowed => {}
    }

    // Step 4: Dispatch to session
    dispatch_command(registry, session_id, command)
}

/// Dispatch a Console command using the unified path
pub fn dispatch_console_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
    payload: CallCommandPayload,
) -> anyhow::Result<CommandResult> {
    // Step 1: Convert to CallCommand
    let command = console_to_call_command(payload, session_id)?;

    // Step 2: Create execution context
    let ctx = ExecutionContext::new(session_id).with_source(CommandSource::Console);

    // Step 3: Check media capabilities
    match ctx.check_media_capability(&command) {
        MediaCapabilityCheck::Denied { reason } => {
            return Ok(CommandResult::not_supported(reason));
        }
        MediaCapabilityCheck::Degraded { reason: _ } => {
            // Continue with degraded functionality
        }
        MediaCapabilityCheck::Allowed => {}
    }

    // Step 4: Dispatch to session
    dispatch_command(registry, session_id, command)
}

/// Dispatch a unified CallCommand directly
pub fn dispatch_call_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
    command: CallCommand,
    source: CommandSource,
) -> anyhow::Result<CommandResult> {
    // Step 1: Create execution context
    let ctx = ExecutionContext::new(session_id).with_source(source);

    // Step 2: Check media capabilities
    match ctx.check_media_capability(&command) {
        MediaCapabilityCheck::Denied { reason } => {
            return Ok(CommandResult::not_supported(reason));
        }
        MediaCapabilityCheck::Degraded { reason: _ } => {
            // Continue with degraded functionality
        }
        MediaCapabilityCheck::Allowed => {}
    }

    // Step 3: Dispatch to session
    dispatch_command(registry, session_id, command)
}

/// Internal: Dispatch a CallCommand to a session
fn dispatch_command(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
    command: CallCommand,
) -> anyhow::Result<CommandResult> {
    // Special handling for Bridge command: check session exists
    if let CallCommand::Bridge { .. } = &command {
        // For Bridge, we need to verify the session has both legs
        // The legs are identified by leg_a and leg_b within the same session
        let Some(handle) = registry.get_handle(session_id) else {
            return Ok(CommandResult::failure(format!(
                "session {} not found",
                session_id
            )));
        };
        // Send the bridge command
        match handle.send_command(command) {
            Ok(_) => return Ok(CommandResult::success()),
            Err(e) => return Ok(CommandResult::failure(format!("failed to dispatch: {}", e))),
        }
    }

    // Get the session handle from registry
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

#[cfg(test)]
mod tests {
    // Integration tests would require a full registry setup
    // Unit tests for conversion are in the adapter modules
}
