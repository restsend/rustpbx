//! CommandExecutor trait - unified command execution interface

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
    /// Optional structured data payload
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl CommandResult {
    /// Create a successful result
    pub fn success() -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: None,
            data: None,
        }
    }

    /// Create a successful result with affected leg
    pub fn success_with_leg(leg: LegId) -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: Some(leg),
            data: None,
        }
    }

    /// Create a failed result
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            affected_leg: None,
            data: None,
        }
    }

    /// Create a successful result with structured data
    pub fn success_with_data(data: serde_json::Value) -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: None,
            data: Some(data),
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
}

impl ExecutionContext {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            media_profile: MediaRuntimeProfile::default(),
        }
    }

    pub fn with_media_profile(mut self, profile: MediaRuntimeProfile) -> Self {
        self.media_profile = profile;
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
            | CallCommand::SupervisorBarge { .. }
            | CallCommand::SupervisorTakeover { .. } => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::domain::MediaSource;

    #[test]
    fn command_result_success() {
        let result = CommandResult::success();
        assert!(result.success);
        assert!(result.message.is_none());
    }

    #[test]
    fn command_result_failure() {
        let result = CommandResult::failure("test error");
        assert!(!result.success);
        assert_eq!(result.message, Some("test error".to_string()));
    }

    #[test]
    fn execution_context_media_check_signaling() {
        let ctx =
            ExecutionContext::new("session-1").with_media_profile(MediaRuntimeProfile::degraded());

        // Signaling-only commands should always be allowed
        let cmd = CallCommand::Answer {
            leg_id: LegId::new("leg-1"),
        };
        assert!(matches!(
            ctx.check_media_capability(&cmd),
            MediaCapabilityCheck::Allowed
        ));
    }

    #[test]
    fn execution_context_media_check_play_bypass() {
        let ctx =
            ExecutionContext::new("session-1").with_media_profile(MediaRuntimeProfile::degraded());

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
        let ctx =
            ExecutionContext::new("session-1").with_media_profile(MediaRuntimeProfile::degraded());

        let cmd = CallCommand::StartRecording {
            config: crate::call::domain::RecordConfig {
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
                channels: None,
                mono_caller_only: None,
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
        let ctx =
            ExecutionContext::new("session-1").with_media_profile(MediaRuntimeProfile::default()); // Anchored by default

        let cmd = CallCommand::StartRecording {
            config: crate::call::domain::RecordConfig {
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
                channels: None,
                mono_caller_only: None,
            },
        };

        assert!(matches!(
            ctx.check_media_capability(&cmd),
            MediaCapabilityCheck::Allowed
        ));
    }
}
