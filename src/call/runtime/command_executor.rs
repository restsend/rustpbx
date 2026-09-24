//! Command result DTOs and the media-capability check used by the session's
//! unified command path (`SipSession::execute_command`).

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
    /// Machine-readable failure kind, set on failures so callers can branch
    /// without string matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<CommandFailureKind>,
}

/// Machine-readable classification of a failed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandFailureKind {
    /// The command is not convertible to the unified dispatch path; the
    /// caller should fall back to its legacy handler.
    NotSupported,
    /// No live session for the target id.
    SessionNotFound,
    /// The session rejected or could not enqueue the command.
    DispatchFailed,
    /// Media capability check denied the command (e.g. bypass mode).
    MediaDenied,
}

impl CommandResult {
    /// Create a successful result
    pub fn success() -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: None,
            data: None,
            failure_kind: None,
        }
    }

    /// Create a successful result with affected leg
    pub fn success_with_leg(leg: LegId) -> Self {
        Self {
            success: true,
            message: None,
            affected_leg: Some(leg),
            data: None,
            failure_kind: None,
        }
    }

    /// Create a failed result
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            affected_leg: None,
            data: None,
            failure_kind: None,
        }
    }

    /// Create a failed result with a machine-readable failure kind
    pub fn failure_with_kind(message: impl Into<String>, kind: CommandFailureKind) -> Self {
        Self {
            success: false,
            message: Some(message.into()),
            affected_leg: None,
            data: None,
            failure_kind: Some(kind),
        }
    }
}

/// Check if the command can be executed with the session's media capabilities
pub fn check_media_capability(
    media_profile: &MediaRuntimeProfile,
    cmd: &CallCommand,
) -> MediaCapabilityCheck {
    if cmd.is_signaling_only() {
        return MediaCapabilityCheck::Allowed;
    }

    if !cmd.requires_media() {
        return MediaCapabilityCheck::Allowed;
    }

    // Check specific media requirements
    match cmd {
        CallCommand::Play { .. } => {
            if media_profile.can_play() {
                MediaCapabilityCheck::Allowed
            } else {
                MediaCapabilityCheck::Degraded {
                    reason: "playback not supported in bypass mode".to_string(),
                }
            }
        }
        CallCommand::StartRecording { .. } => {
            if media_profile.can_record() {
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
            if media_profile.can_supervise() {
                MediaCapabilityCheck::Allowed
            } else {
                MediaCapabilityCheck::Denied {
                    reason: "supervisor modes not supported in bypass mode".to_string(),
                }
            }
        }
        CallCommand::StartTranscription { .. } => {
            // Like supervision, transcription taps decoded leg media —
            // impossible in bypass mode (no MediaBridge).
            if media_profile.can_supervise() {
                MediaCapabilityCheck::Allowed
            } else {
                MediaCapabilityCheck::Denied {
                    reason: "transcription not supported in bypass mode".to_string(),
                }
            }
        }
        CallCommand::Hold { music: Some(_), .. } => {
            if media_profile.supports_media_injection {
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
            &MediaRuntimeProfile::degraded();

        // Signaling-only commands should always be allowed
        let cmd = CallCommand::Answer {
            leg_id: LegId::new("leg-1"),
        };
        assert!(matches!(
            check_media_capability(ctx, &cmd),
            MediaCapabilityCheck::Allowed
        ));
    }

    #[test]
    fn execution_context_media_check_play_bypass() {
        let ctx =
            &MediaRuntimeProfile::degraded();

        let cmd = CallCommand::Play {
            leg_id: None,
            source: MediaSource::file("test.wav"),
            options: None,
        };

        match check_media_capability(ctx, &cmd) {
            MediaCapabilityCheck::Degraded { reason } => {
                assert!(reason.contains("bypass"));
            }
            _ => panic!("Expected Degraded"),
        }
    }

    #[test]
    fn execution_context_media_check_record_bypass() {
        let ctx =
            &MediaRuntimeProfile::degraded();

        let cmd = CallCommand::StartRecording {
            config: crate::call::domain::RecordConfig {
                unique_id: None,
                discard: None,
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
                channels: None,
                mono_caller_only: None,
                segment_type: None,
                segment_id: None,
                label: None,
                notify_app: None,
            },
        };

        match check_media_capability(ctx, &cmd) {
            MediaCapabilityCheck::Denied { reason } => {
                assert!(reason.contains("recording"));
            }
            _ => panic!("Expected Denied"),
        }
    }

    #[test]
    fn execution_context_media_check_record_anchored() {
        let ctx =
            &MediaRuntimeProfile::default(); // Anchored by default

        let cmd = CallCommand::StartRecording {
            config: crate::call::domain::RecordConfig {
                unique_id: None,
                discard: None,
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
                channels: None,
                mono_caller_only: None,
                segment_type: None,
                segment_id: None,
                label: None,
                notify_app: None,
            },
        };

        assert!(matches!(
            check_media_capability(ctx, &cmd),
            MediaCapabilityCheck::Allowed
        ));
    }
}
