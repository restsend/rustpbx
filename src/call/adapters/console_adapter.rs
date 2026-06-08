//! Console/HTTP Command Adapter
//!
//! Converts `CallCommandPayload` (from HTTP API) to unified `CallCommand`.

use crate::call::domain::*;
use crate::callrecord::CallRecordHangupReason;
use crate::console::handlers::call_control::{CallCommandPayload, ConsoleMediaSource};
use anyhow::Result;

fn convert_console_source(src: ConsoleMediaSource) -> MediaSource {
    match src {
        ConsoleMediaSource::File { path } => MediaSource::file(path),
        ConsoleMediaSource::Url { url } => MediaSource::url(url),
        ConsoleMediaSource::Silence => MediaSource::Silence,
        ConsoleMediaSource::Tone {
            frequency,
            duration_ms,
        } => MediaSource::Tone {
            frequency,
            duration_ms,
        },
    }
}

/// Convert hangup reason string to CallRecordHangupReason
fn parse_hangup_reason(reason: Option<&str>) -> Option<CallRecordHangupReason> {
    reason.and_then(|r| match r.to_lowercase().as_str() {
        "by_caller" | "caller" => Some(CallRecordHangupReason::ByCaller),
        "by_callee" | "callee" => Some(CallRecordHangupReason::ByCallee),
        "by_system" | "system" => Some(CallRecordHangupReason::BySystem),
        "no_answer" => Some(CallRecordHangupReason::NoAnswer),
        "rejected" => Some(CallRecordHangupReason::Rejected),
        "canceled" => Some(CallRecordHangupReason::Canceled),
        "failed" => Some(CallRecordHangupReason::Failed),
        _ => None,
    })
}

/// Convert Console CallCommandPayload to unified CallCommand
///
/// # Arguments
/// * `payload` - The console command payload
/// * `session_id` - The session ID context
///
/// # Returns
/// * `Ok(CallCommand)` - Successfully converted command
/// * `Err` - Conversion failed
pub fn console_to_call_command(
    payload: CallCommandPayload,
    session_id: &str,
) -> Result<CallCommand> {
    match payload {
        CallCommandPayload::Hangup {
            reason,
            code,
            initiator,
        } => {
            let cdr_reason = parse_hangup_reason(reason.as_deref());
            let mut cmd =
                HangupCommand::local(initiator.as_deref().unwrap_or("console"), cdr_reason, code);
            cmd = cmd.with_cascade(HangupCascade::All);
            Ok(CallCommand::Hangup(cmd))
        }

        CallCommandPayload::Accept { callee, sdp } => {
            // Accept is similar to Answer but with additional context
            // For now, we map it to Answer with the leg being the session itself
            // The callee and sdp fields are used internally by the session
            let _ = (callee, sdp); // Acknowledge but don't use for now
            Ok(CallCommand::Answer {
                leg_id: LegId::new(session_id),
            })
        }

        CallCommandPayload::Transfer { target } => Ok(CallCommand::Transfer {
            leg_id: LegId::new(session_id),
            target,
            attended: false,
        }),

        CallCommandPayload::Mute { track_id } => Ok(CallCommand::MuteTrack { track_id }),

        CallCommandPayload::Unmute { track_id } => Ok(CallCommand::UnmuteTrack { track_id }),

        CallCommandPayload::Play {
            source,
            leg_id,
            interrupt_on_dtmf,
            loop_playback,
        } => Ok(CallCommand::Play {
            leg_id: leg_id.map(LegId::new).or(Some(LegId::new(session_id))),
            source: convert_console_source(source),
            options: Some(PlayOptions {
                interrupt_on_dtmf,
                loop_playback,
                ..Default::default()
            }),
        }),

        CallCommandPayload::StopPlayback { leg_id } => Ok(CallCommand::StopPlayback {
            leg_id: leg_id.map(LegId::new),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hangup_conversion() {
        let payload = CallCommandPayload::Hangup {
            reason: Some("normal_clearing".to_string()),
            code: Some(200),
            initiator: Some("admin".to_string()),
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::Hangup(hangup_cmd) = cmd {
            assert_eq!(hangup_cmd.code, Some(200));
        } else {
            panic!("Expected Hangup command");
        }
    }

    #[test]
    fn test_transfer_conversion() {
        let payload = CallCommandPayload::Transfer {
            target: "sip:1001@example.com".to_string(),
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::Transfer {
            leg_id,
            target,
            attended,
        } = cmd
        {
            assert_eq!(leg_id.as_str(), "session-123");
            assert_eq!(target, "sip:1001@example.com");
            assert!(!attended);
        } else {
            panic!("Expected Transfer command");
        }
    }

    #[test]
    fn test_mute_conversion() {
        let payload = CallCommandPayload::Mute {
            track_id: "track-audio-1".to_string(),
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::MuteTrack { track_id } = cmd {
            assert_eq!(track_id, "track-audio-1");
        } else {
            panic!("Expected MuteTrack command");
        }
    }

    #[test]
    fn test_play_file_conversion() {
        let payload = CallCommandPayload::Play {
            source: ConsoleMediaSource::File {
                path: "/audio/announce.wav".to_string(),
            },
            leg_id: None,
            interrupt_on_dtmf: true,
            loop_playback: false,
        };
        let cmd = console_to_call_command(payload, "session-abc").unwrap();
        if let CallCommand::Play {
            leg_id,
            source,
            options,
        } = cmd
        {
            assert_eq!(leg_id.as_ref().unwrap().as_str(), "session-abc");
            assert!(matches!(source, MediaSource::File { .. }));
            let opts = options.unwrap();
            assert!(opts.interrupt_on_dtmf);
            assert!(!opts.loop_playback);
        } else {
            panic!("Expected Play command");
        }
    }

    #[test]
    fn test_play_with_leg_id() {
        let payload = CallCommandPayload::Play {
            source: ConsoleMediaSource::Silence,
            leg_id: Some("leg-callee".to_string()),
            interrupt_on_dtmf: false,
            loop_playback: true,
        };
        let cmd = console_to_call_command(payload, "session-abc").unwrap();
        if let CallCommand::Play {
            leg_id,
            source,
            options,
        } = cmd
        {
            assert_eq!(leg_id.as_ref().unwrap().as_str(), "leg-callee");
            assert!(matches!(source, MediaSource::Silence));
            let opts = options.unwrap();
            assert!(opts.loop_playback);
            assert!(!opts.interrupt_on_dtmf);
        } else {
            panic!("Expected Play command");
        }
    }

    #[test]
    fn test_stop_playback_conversion() {
        let payload = CallCommandPayload::StopPlayback { leg_id: None };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::StopPlayback { leg_id } = cmd {
            assert!(leg_id.is_none());
        } else {
            panic!("Expected StopPlayback command");
        }
    }

    #[test]
    fn test_stop_playback_with_leg_conversion() {
        let payload = CallCommandPayload::StopPlayback {
            leg_id: Some("leg-callee".to_string()),
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::StopPlayback { leg_id } = cmd {
            assert_eq!(leg_id.unwrap().as_str(), "leg-callee");
        } else {
            panic!("Expected StopPlayback command");
        }
    }
}
