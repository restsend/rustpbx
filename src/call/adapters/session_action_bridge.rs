//! Session Action Bridge
//!
//! This module provides the conversion layer between unified `CallCommand`
//! and the existing `SessionAction` type. This is a permanent adapter
//! that enables the unified runtime to communicate with the existing
//! session implementation.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────┐     ┌──────────────┐     ┌───────────────────┐
//! │  RWI/Console │ --> │  CallCommand │ --> │   SessionAction   │ --> CallSession
//! │   Adapters   │     │   (unified)  │     │    (this module)  │
//! └─────────────┘     └──────────────┘     └───────────────────┘
//! ```
//!
//! ## Usage
//!
//! This module is used by:
//! - `command_dispatch.rs` - For unified command dispatch
//! - `session_action_executor.rs` - For CommandExecutor implementation
//!
//! ## Design Notes
//!
//! Not all `CallCommand` variants have a direct `SessionAction` equivalent.
//! In such cases, the conversion returns an error.

use crate::call::domain::*;
use crate::proxy::proxy_call::state::SessionAction;
use crate::callrecord::CallRecordHangupReason;
use anyhow::Result;

use super::AdapterError;

/// Convert unified CallCommand to SessionAction (legacy)
///
/// This is a temporary bridge for migration purposes.
/// Not all CallCommand variants have a direct SessionAction equivalent.
///
/// # Arguments
/// * `cmd` - The unified CallCommand
///
/// # Returns
/// * `Ok(SessionAction)` - Successfully converted action
/// * `Err` - No equivalent SessionAction exists
pub fn call_command_to_session_action(cmd: CallCommand) -> Result<SessionAction> {
    match cmd {
        // ========================================================================
        // Basic Call Control
        // ========================================================================
        CallCommand::Answer { leg_id: _ } => {
            // Answer requires additional context (callee, sdp, dialog_id)
            // This is a simplified mapping
            Ok(SessionAction::AcceptCall {
                callee: None,
                sdp: None,
                dialog_id: None,
            })
        }

        CallCommand::Reject { leg_id: _, reason } => {
            // Reject maps to Hangup with appropriate code
            Ok(SessionAction::Hangup {
                reason: reason
                    .as_deref()
                    .and_then(|r| match r.to_lowercase().as_str() {
                        "busy" => Some(CallRecordHangupReason::Failed),
                        "declined" | "rejected" => Some(CallRecordHangupReason::Rejected),
                        _ => Some(CallRecordHangupReason::BySystem),
                    }),
                code: Some(603), // Decline
                initiator: Some("reject".to_string()),
            })
        }

        CallCommand::Ring { leg_id: _, ringback } => {
            let (rb, passthrough) = match ringback {
                Some(RingbackPolicy::PassThrough) => (None, true),
                Some(RingbackPolicy::Replace { source }) => {
                    let path = match source {
                        MediaSource::File { path } => path,
                        _ => String::new(),
                    };
                    (Some(path), false)
                }
                Some(RingbackPolicy::Block) => (None, false),
                _ => (None, true),
            };
            Ok(SessionAction::StartRinging {
                ringback: rb,
                passthrough,
            })
        }

        CallCommand::Hangup(hangup_cmd) => Ok(SessionAction::Hangup {
            reason: hangup_cmd.reason,
            code: hangup_cmd.code,
            initiator: Some("local".to_string()),
        }),

        // ========================================================================
        // Bridging
        // ========================================================================
        CallCommand::Bridge { leg_a: _, leg_b, .. } => {
            // Bridge currently uses target_session_id, not leg-based
            // This is a simplified mapping using leg_b as target
            Ok(SessionAction::BridgeTo {
                target_session_id: leg_b.into(),
            })
        }

        CallCommand::Unbridge { leg_id: _ } => Ok(SessionAction::Unbridge),

        // ========================================================================
        // Transfer
        // ========================================================================
        CallCommand::Transfer {
            leg_id: _,
            target,
            attended: _,
        } => Ok(SessionAction::TransferTarget(target)),

        CallCommand::TransferComplete { consult_leg: _ }
        | CallCommand::TransferCancel { consult_leg: _ } => {
            // These require complex multi-leg coordination
            // Not directly mappable to single SessionAction
            Err(AdapterError::NotSupported(
                "transfer complete/cancel requires multi-leg coordination".to_string(),
            )
            .into())
        }

        // ========================================================================
        // Hold
        // ========================================================================
        CallCommand::Hold { leg_id: _, music } => Ok(SessionAction::Hold {
            music_source: music.and_then(|m| match m {
                MediaSource::File { path } => Some(path),
                _ => None,
            }),
        }),

        CallCommand::Unhold { leg_id: _ } => Ok(SessionAction::Unhold),

        // ========================================================================
        // Media Operations
        // ========================================================================
        CallCommand::Play {
            leg_id: _,
            source,
            options,
        } => {
            let path = match source {
                MediaSource::File { path } => path,
                _ => return Err(AdapterError::NotSupported("non-file media source".to_string()).into()),
            };
            let opts = options.unwrap_or_default();
            Ok(SessionAction::PlayPrompt {
                audio_file: path,
                send_progress: opts.send_progress,
                await_completion: opts.await_completion,
                track_id: opts.track_id,
                loop_playback: opts.loop_playback,
                interrupt_on_dtmf: opts.interrupt_on_dtmf,
            })
        }

        CallCommand::StopPlayback { leg_id: _ } => Ok(SessionAction::StopPlayback),

        CallCommand::SendDtmf { .. } => {
            // DTMF sending is not directly supported in SessionAction
            Err(AdapterError::NotSupported("dtmf sending".to_string()).into())
        }

        // ========================================================================
        // Recording
        // ========================================================================
        CallCommand::StartRecording { config } => Ok(SessionAction::StartRecording {
            path: config.path,
            max_duration: config.max_duration_secs.map(|s| std::time::Duration::from_secs(s as u64)),
            beep: config.beep,
        }),

        CallCommand::PauseRecording => Ok(SessionAction::PauseRecording),

        CallCommand::ResumeRecording => Ok(SessionAction::ResumeRecording),

        CallCommand::StopRecording => Ok(SessionAction::StopRecording),

        // ========================================================================
        // Supervisor Operations
        // ========================================================================
        CallCommand::SupervisorListen { target_leg, .. } => Ok(SessionAction::SupervisorListen {
            target_session_id: target_leg.into(),
        }),

        CallCommand::SupervisorWhisper { target_leg, .. } => Ok(SessionAction::SupervisorWhisper {
            target_session_id: target_leg.into(),
        }),

        CallCommand::SupervisorBarge { target_leg, .. } => Ok(SessionAction::SupervisorBarge {
            target_session_id: target_leg.into(),
        }),

        CallCommand::SupervisorStop { .. } => Ok(SessionAction::SupervisorStop),

        // ========================================================================
        // Internal Operations
        // ========================================================================
        CallCommand::HandleReInvite { leg_id: _, sdp } => {
            // HandleReInvite uses (method, sdp) tuple
            Ok(SessionAction::HandleReInvite("INVITE".to_string(), sdp))
        }

        CallCommand::RefreshSession => Ok(SessionAction::RefreshSession),

        CallCommand::MuteTrack { track_id } => Ok(SessionAction::MuteTrack(track_id)),

        CallCommand::UnmuteTrack { track_id } => Ok(SessionAction::UnmuteTrack(track_id)),

        // ========================================================================
        // Not Yet Supported
        // ========================================================================
        CallCommand::ConferenceCreate { .. }
        | CallCommand::ConferenceAdd { .. }
        | CallCommand::ConferenceRemove { .. }
        | CallCommand::ConferenceMute { .. }
        | CallCommand::ConferenceUnmute { .. }
        | CallCommand::ConferenceDestroy { .. } => {
            Err(AdapterError::NotSupported("conference commands".to_string()).into())
        }

        CallCommand::QueueEnqueue { .. } | CallCommand::QueueDequeue { .. } => {
            Err(AdapterError::NotSupported("queue commands".to_string()).into())
        }

        CallCommand::StartApp { .. }
        | CallCommand::StopApp { .. }
        | CallCommand::InjectAppEvent { .. } => {
            Err(AdapterError::NotSupported("app commands".to_string()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hangup_to_session_action() {
        let cmd = CallCommand::Hangup(HangupCommand::all(
            Some(CallRecordHangupReason::BySystem),
            Some(200),
        ));
        let action = call_command_to_session_action(cmd).unwrap();
        if let SessionAction::Hangup { code, .. } = action {
            assert_eq!(code, Some(200));
        } else {
            panic!("Expected Hangup action");
        }
    }

    #[test]
    fn test_hold_to_session_action() {
        let cmd = CallCommand::Hold {
            leg_id: LegId::new("leg-1"),
            music: Some(MediaSource::file("music.wav")),
        };
        let action = call_command_to_session_action(cmd).unwrap();
        if let SessionAction::Hold { music_source } = action {
            assert_eq!(music_source, Some("music.wav".to_string()));
        } else {
            panic!("Expected Hold action");
        }
    }

    #[test]
    fn test_play_to_session_action() {
        let cmd = CallCommand::Play {
            leg_id: None,
            source: MediaSource::file("prompt.wav"),
            options: Some(PlayOptions {
                interrupt_on_dtmf: true,
                ..Default::default()
            }),
        };
        let action = call_command_to_session_action(cmd).unwrap();
        if let SessionAction::PlayPrompt {
            audio_file,
            interrupt_on_dtmf,
            ..
        } = action
        {
            assert_eq!(audio_file, "prompt.wav");
            assert!(interrupt_on_dtmf);
        } else {
            panic!("Expected PlayPrompt action");
        }
    }

    #[test]
    fn test_unsupported_command() {
        let cmd = CallCommand::StartApp {
            app_name: "ivr".to_string(),
            params: None,
            auto_answer: false,
        };
        let result = call_command_to_session_action(cmd);
        assert!(result.is_err());
    }
}
