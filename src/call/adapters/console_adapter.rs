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
    _session_id: &str,
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
            // The incoming leg of a proxied call is always the "caller" leg.
            // NOTE: the unified `Answer` command carries only a leg_id; the SDP
            // is resolved internally by the session (see process_command Answer
            // branch). Map to the caller leg so a real SIP 200 OK is sent.
            let _ = (callee, sdp);
            Ok(CallCommand::Answer {
                leg_id: LegId::new("caller"),
            })
        }

        CallCommandPayload::Transfer { target, attended } => Ok(CallCommand::Transfer {
            leg_id: LegId::new("caller"),
            target,
            attended: attended.unwrap_or(false),
        }),

        CallCommandPayload::Hold { leg_id } => Ok(CallCommand::Hold {
            leg_id: LegId::new(leg_id.as_deref().unwrap_or("caller")),
            music: None,
        }),

        CallCommandPayload::Unhold { leg_id } => Ok(CallCommand::Unhold {
            leg_id: LegId::new(leg_id.as_deref().unwrap_or("caller")),
        }),

        CallCommandPayload::SendDtmf { digits, leg_id } => Ok(CallCommand::SendDtmf {
            leg_id: LegId::new(leg_id.as_deref().unwrap_or("caller")),
            digits,
        }),

        CallCommandPayload::Mute { track_id } => Ok(CallCommand::MuteTrack { track_id }),

        CallCommandPayload::Unmute { track_id } => Ok(CallCommand::UnmuteTrack { track_id }),

        CallCommandPayload::Play {
            source,
            leg_id,
            interrupt_on_dtmf,
            loop_playback,
        } => Ok(CallCommand::Play {
            leg_id: Some(LegId::new(leg_id.as_deref().unwrap_or("caller"))),
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

        CallCommandPayload::StartRecording { path, format } => Ok(CallCommand::StartRecording {
            config: RecordConfig {
                path: path.unwrap_or_else(|| "recordings/console-recording.wav".to_string()),
                max_duration_secs: None,
                beep: true,
                format,
            },
        }),

        CallCommandPayload::StopRecording => Ok(CallCommand::StopRecording),
        CallCommandPayload::PauseRecording => Ok(CallCommand::PauseRecording),
        CallCommandPayload::ResumeRecording => Ok(CallCommand::ResumeRecording),
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
            attended: None,
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::Transfer {
            leg_id,
            target,
            attended,
        } = cmd
        {
            assert_eq!(leg_id.as_str(), "caller");
            assert_eq!(target, "sip:1001@example.com");
            assert!(!attended);
        } else {
            panic!("Expected Transfer command");
        }
    }

    #[test]
    fn test_transfer_attended_conversion() {
        let payload = CallCommandPayload::Transfer {
            target: "sip:1001@example.com".to_string(),
            attended: Some(true),
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::Transfer { attended, .. } = cmd {
            assert!(attended);
        } else {
            panic!("Expected Transfer command");
        }
    }

    #[test]
    fn test_accept_conversion_maps_to_caller_leg() {
        let payload = CallCommandPayload::Accept {
            callee: None,
            sdp: None,
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::Answer { leg_id } = cmd {
            assert_eq!(leg_id.as_str(), "caller");
        } else {
            panic!("Expected Answer command");
        }
    }

    #[test]
    fn test_hold_unhold_conversion() {
        let hold = console_to_call_command(
            CallCommandPayload::Hold { leg_id: None },
            "session-123",
        )
        .unwrap();
        if let CallCommand::Hold { leg_id, music } = hold {
            assert_eq!(leg_id.as_str(), "caller");
            assert!(music.is_none());
        } else {
            panic!("Expected Hold command");
        }

        let unhold = console_to_call_command(
            CallCommandPayload::Unhold {
                leg_id: Some("callee".to_string()),
            },
            "session-123",
        )
        .unwrap();
        if let CallCommand::Unhold { leg_id } = unhold {
            assert_eq!(leg_id.as_str(), "callee");
        } else {
            panic!("Expected Unhold command");
        }
    }

    #[test]
    fn test_send_dtmf_conversion() {
        let payload = CallCommandPayload::SendDtmf {
            digits: "123".to_string(),
            leg_id: None,
        };
        let cmd = console_to_call_command(payload, "session-123").unwrap();
        if let CallCommand::SendDtmf { leg_id, digits } = cmd {
            assert_eq!(leg_id.as_str(), "caller");
            assert_eq!(digits, "123");
        } else {
            panic!("Expected SendDtmf command");
        }
    }

    #[test]
    fn test_recording_conversion() {
        let start = console_to_call_command(
            CallCommandPayload::StartRecording {
                path: Some("/tmp/a.wav".to_string()),
                format: Some("wav".to_string()),
            },
            "session-123",
        )
        .unwrap();
        if let CallCommand::StartRecording { config } = start {
            assert_eq!(config.path, "/tmp/a.wav");
            assert_eq!(config.format.as_deref(), Some("wav"));
        } else {
            panic!("Expected StartRecording command");
        }

        let stop = console_to_call_command(CallCommandPayload::StopRecording, "session-123").unwrap();
        assert!(matches!(stop, CallCommand::StopRecording));
        let pause = console_to_call_command(CallCommandPayload::PauseRecording, "session-123").unwrap();
        assert!(matches!(pause, CallCommand::PauseRecording));
        let resume = console_to_call_command(CallCommandPayload::ResumeRecording, "session-123").unwrap();
        assert!(matches!(resume, CallCommand::ResumeRecording));
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
            // Omitted leg_id must default to "caller", never the session id.
            assert_eq!(leg_id.as_ref().unwrap().as_str(), "caller");
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
    fn test_play_both_legs_conversion() {
        let payload = CallCommandPayload::Play {
            source: ConsoleMediaSource::Url {
                url: "http://example.com/file.wav".to_string(),
            },
            leg_id: Some("both".to_string()),
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
            assert_eq!(leg_id.as_ref().unwrap().as_str(), "both");
            assert!(matches!(source, MediaSource::Url { .. }));
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
