//! CommandExecutor trait - unified command execution interface

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::call::domain::{CallCommand, LegId, MediaRuntimeProfile};

/// Result of command execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// Whether the command was executed successfully
    pub success: bool,
    /// Optional message (error or status)
    pub message: Option<String>,
    /// The leg that was affected (if any)
    pub affected_leg: Option<LegId>,
    /// Whether media capabilities were degraded
    pub media_degraded: bool,
    /// Degradation reason (if media was degraded)
    pub degradation_reason: Option<String>,
}

impl CommandResult {
    /// Create a successful result
    pub fn success() -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: None,
            media_degraded: false,
            degradation_reason: None,
        }
    }

    /// Create a successful result with affected leg
    pub fn success_with_leg(leg: LegId) -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: Some(leg),
            media_degraded: false,
            degradation_reason: None,
        }
    }

    /// Create a failed result
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            affected_leg: None,
            media_degraded: false,
            degradation_reason: None,
        }
    }

    /// Create a degraded result (command succeeded but with limited capability)
    pub fn degraded(reason: impl Into<String>) -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: None,
            media_degraded: true,
            degradation_reason: Some(reason.into()),
        }
    }

    /// Create a not supported result (for bypass mode media commands)
    pub fn not_supported(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            affected_leg: None,
            media_degraded: false,
            degradation_reason: None,
        }
    }
}

/// Context for command execution
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    /// The session ID
    pub session_id: String,
    /// Media runtime profile for capability checks
    pub media_profile: MediaRuntimeProfile,
    /// Source of the command (for logging/tracking)
    pub source: CommandSource,
    /// Optional command ID for tracking
    pub command_id: Option<String>,
}

impl ExecutionContext {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            media_profile: MediaRuntimeProfile::default(),
            source: CommandSource::Internal,
            command_id: None,
        }
    }

    pub fn with_media_profile(mut self, profile: MediaRuntimeProfile) -> Self {
        self.media_profile = profile;
        self
    }

    pub fn with_source(mut self, source: CommandSource) -> Self {
        self.source = source;
        self
    }

    pub fn with_command_id(mut self, id: impl Into<String>) -> Self {
        self.command_id = Some(id.into());
        self
    }

    /// Check if the command can be executed with current media capabilities
    pub fn check_media_capability(&self, cmd: &CallCommand) -> MediaCapabilityCheck {
        if cmd.is_signaling_only() {
            return MediaCapabilityCheck::Allowed;
        }

        if !cmd.requires_media() {
            return MediaCapabilityCheck::Allowed;
        }

        // Check specific media requirements
        match cmd {
            CallCommand::Play { .. } => {
                if self.media_profile.can_play() {
                    MediaCapabilityCheck::Allowed
                } else {
                    MediaCapabilityCheck::Degraded {
                        reason: "playback not supported in bypass mode".to_string(),
                    }
                }
            }
            CallCommand::StartRecording { .. } => {
                if self.media_profile.can_record() {
                    MediaCapabilityCheck::Allowed
                } else {
                    MediaCapabilityCheck::Denied {
                        reason: "recording not supported in bypass mode".to_string(),
                    }
                }
            }
            CallCommand::SupervisorListen { .. }
            | CallCommand::SupervisorWhisper { .. }
            | CallCommand::SupervisorBarge { .. } => {
                if self.media_profile.can_supervise() {
                    MediaCapabilityCheck::Allowed
                } else {
                    MediaCapabilityCheck::Denied {
                        reason: "supervisor modes not supported in bypass mode".to_string(),
                    }
                }
            }
            CallCommand::Hold { music: Some(_), .. } => {
                if self.media_profile.supports_media_injection {
                    MediaCapabilityCheck::Allowed
                } else {
                    // Hold itself works, but music won't play
                    MediaCapabilityCheck::Degraded {
                        reason: "hold music not supported in bypass mode".to_string(),
                    }
                }
            }
            _ => MediaCapabilityCheck::Allowed,
        }
    }
}

/// Result of media capability check
#[derive(Debug, Clone)]
pub enum MediaCapabilityCheck {
    /// Command can be executed fully
    Allowed,
    /// Command can be executed but with degraded functionality
    Degraded { reason: String },
    /// Command cannot be executed due to capability limitations
    Denied { reason: String },
}

/// Source of the command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    /// RWI (WebSocket) interface
    Rwi,
    /// Console (HTTP) interface
    Console,
    /// Internal system
    Internal,
    /// SIP signaling
    Sip,
}

impl std::fmt::Display for CommandSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandSource::Rwi => write!(f, "rwi"),
            CommandSource::Console => write!(f, "console"),
            CommandSource::Internal => write!(f, "internal"),
            CommandSource::Sip => write!(f, "sip"),
        }
    }
}

/// Unified command executor trait
///
/// This trait provides a single interface for executing commands on sessions.
/// Implementations can wrap existing session logic or provide new unified runtime.
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Execute a command on a session
    ///
    /// # Arguments
    /// * `ctx` - Execution context (session ID, media profile, etc.)
    /// * `command` - The command to execute
    ///
    /// # Returns
    /// * `Ok(CommandResult)` - Command execution result
    /// * `Err` - System error (not command failure)
    async fn execute(&self, ctx: ExecutionContext, command: CallCommand) -> anyhow::Result<CommandResult>;

    /// Check if a session exists
    async fn session_exists(&self, session_id: &str) -> bool;

    /// Get the media profile for a session
    async fn get_media_profile(&self, session_id: &str) -> Option<MediaRuntimeProfile>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::domain::MediaSource;

    #[test]
    fn command_result_success() {
        let result = CommandResult::success();
        assert!(result.success);
        assert!(result.message.is_none());
        assert!(!result.media_degraded);
    }

    #[test]
    fn command_result_failure() {
        let result = CommandResult::failure("test error");
        assert!(!result.success);
        assert_eq!(result.message, Some("test error".to_string()));
    }

    #[test]
    fn command_result_degraded() {
        let result = CommandResult::degraded("bypass mode");
        assert!(result.success);
        assert!(result.media_degraded);
        assert_eq!(result.degradation_reason, Some("bypass mode".to_string()));
    }

    #[test]
    fn execution_context_media_check_signaling() {
        let ctx = ExecutionContext::new("session-1")
            .with_media_profile(MediaRuntimeProfile::degraded());

        // Signaling-only commands should always be allowed
        let cmd = CallCommand::Answer {
            leg_id: LegId::new("leg-1"),
        };
        assert!(matches!(ctx.check_media_capability(&cmd), MediaCapabilityCheck::Allowed));
    }

    #[test]
    fn execution_context_media_check_play_bypass() {
        let ctx = ExecutionContext::new("session-1")
            .with_media_profile(MediaRuntimeProfile::degraded());

        let cmd = CallCommand::Play {
            leg_id: None,
            source: MediaSource::file("test.wav"),
            options: None,
        };

        match ctx.check_media_capability(&cmd) {
            MediaCapabilityCheck::Degraded { reason } => {
                assert!(reason.contains("bypass"));
            }
            _ => panic!("Expected Degraded"),
        }
    }

    #[test]
    fn execution_context_media_check_record_bypass() {
        let ctx = ExecutionContext::new("session-1")
            .with_media_profile(MediaRuntimeProfile::degraded());

        let cmd = CallCommand::StartRecording {
            config: crate::call::domain::RecordConfig {
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
            },
        };

        match ctx.check_media_capability(&cmd) {
            MediaCapabilityCheck::Denied { reason } => {
                assert!(reason.contains("recording"));
            }
            _ => panic!("Expected Denied"),
        }
    }

    #[test]
    fn execution_context_media_check_record_anchored() {
        let ctx = ExecutionContext::new("session-1")
            .with_media_profile(MediaRuntimeProfile::default()); // Anchored by default

        let cmd = CallCommand::StartRecording {
            config: crate::call::domain::RecordConfig {
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
            },
        };

        assert!(matches!(ctx.check_media_capability(&cmd), MediaCapabilityCheck::Allowed));
    }
}
