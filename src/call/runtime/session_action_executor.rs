//! SessionActionExecutor - Bridge to existing SessionAction implementation
//!
//! This executor wraps the existing `CallSession::apply_session_action` path,
//! converting `CallCommand` to `SessionAction` and executing via the legacy runtime.
//!
//! ## Status
//!
//! This is a **migration executor**. It will be replaced by `SipSessionRuntime`
//! once Phase D is complete.
//!
//! NOTE: This file has been updated to work with SipSessionHandle directly.
//! The SessionAction conversion is no longer used since SipSessionHandle
//! accepts CallCommand directly.

use async_trait::async_trait;
use std::sync::Arc;

use crate::call::domain::CallCommand;
use crate::call::domain::MediaRuntimeProfile;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;

use super::{CommandExecutor, CommandResult, ExecutionContext, MediaCapabilityCheck};

/// Executor that bridges to the SipSession runtime
pub struct SessionActionExecutor {
    registry: Arc<ActiveProxyCallRegistry>,
}

impl SessionActionExecutor {
    pub fn new(registry: Arc<ActiveProxyCallRegistry>) -> Self {
        Self { registry }
    }

    /// Apply a CallCommand to a session via SipSessionHandle
    fn apply_command(
        &self,
        session_id: &str,
        command: CallCommand,
    ) -> anyhow::Result<CommandResult> {
        // Get the session handle from registry
        let Some(handle) = self.registry.get_handle(session_id) else {
            return Ok(CommandResult::failure(format!(
                "session {} not found",
                session_id
            )));
        };

        // Send the command directly to SipSessionHandle
        match handle.send_command(command) {
            Ok(_) => Ok(CommandResult::success()),
            Err(e) => Ok(CommandResult::failure(format!("failed to apply command: {}", e))),
        }
    }
}

#[async_trait]
impl CommandExecutor for SessionActionExecutor {
    async fn execute(
        &self,
        ctx: ExecutionContext,
        command: CallCommand,
    ) -> anyhow::Result<CommandResult> {
        // Step 1: Check media capabilities
        match ctx.check_media_capability(&command) {
            MediaCapabilityCheck::Denied { reason } => {
                // Command cannot be executed due to capability limitations
                return Ok(CommandResult::not_supported(reason));
            }
            MediaCapabilityCheck::Degraded { reason } => {
                // Command can execute but with degraded functionality
                // For now, we still try to execute but return degraded result
                let _ = reason; // Acknowledge for now
            }
            MediaCapabilityCheck::Allowed => {
                // Command can execute fully
            }
        }

        // Step 2: Send command directly to SipSessionHandle
        self.apply_command(&ctx.session_id, command)
    }

    async fn session_exists(&self, session_id: &str) -> bool {
        self.registry.get_handle(session_id).is_some()
    }

    async fn get_media_profile(&self, _session_id: &str) -> Option<MediaRuntimeProfile> {
        // For now, return default profile (Anchored)
        // TODO: Get actual media path mode from session state
        Some(MediaRuntimeProfile::default())
    }
}

#[cfg(test)]
mod tests {
    // Note: Integration tests for SessionActionExecutor require a full test setup
    // with ActiveProxyCallRegistry and SipSession. These are tested in the
    // integration_tests module.
    //
    // Unit tests for command conversion are in session_action_bridge.rs
}
