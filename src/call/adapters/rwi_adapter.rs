//! RWI Command Adapter
//!
//! Converts `RwiCommandPayload` to unified `CallCommand`.

use crate::call::domain::*;
use crate::rwi::session::RwiCommandPayload;
use anyhow::Result;

use super::{AdapterError, parse_hangup_reason};

/// Convert RWI command to unified CallCommand
///
/// # Arguments
/// * `payload` - The RWI command payload
/// * `session_id` - Optional session ID context (required for most commands)
///
/// # Returns
/// * `Ok(CallCommand)` - Successfully converted command
/// * `Err` - Conversion failed (missing fields, unsupported command, etc.)
pub fn rwi_to_call_command(
    payload: RwiCommandPayload,
    session_id: Option<&str>,
) -> Result<CallCommand> {
    match payload {
        RwiCommandPayload::Subscribe { .. }
        | RwiCommandPayload::Unsubscribe { .. }
        | RwiCommandPayload::ListCalls
        | RwiCommandPayload::SetVar { .. }
        | RwiCommandPayload::GetVar { .. }
        | RwiCommandPayload::AttachCall { .. }
        | RwiCommandPayload::DetachCall { .. }
        | RwiCommandPayload::SipMessage { .. }
        | RwiCommandPayload::SipNotify { .. }
        | RwiCommandPayload::SipOptionsPing { .. }
        | RwiCommandPayload::SessionResume { .. }
        | RwiCommandPayload::CallResume { .. }
        | RwiCommandPayload::LegAdd { .. }
        | RwiCommandPayload::LegRemove { .. }
        | RwiCommandPayload::AppStart { .. }
        | RwiCommandPayload::AppStop { .. }
        | RwiCommandPayload::AppChain { .. }
        | RwiCommandPayload::ConferenceEnd { .. }
        | RwiCommandPayload::DtmfCollect(_) => {
            Err(AdapterError::NotSupported("session management command".to_string()).into())
        }

        RwiCommandPayload::Originate(_) => Err(AdapterError::NotSupported(
            "originate requires separate handling".to_string(),
        )
        .into()),

        // ========================================================================
        // Basic Call Control
        // ========================================================================
        RwiCommandPayload::Answer { call_id: _ } => {
            // RWI identifies calls by session id, but SipSession legs are named
            // "caller"/"callee" — these commands target the caller leg.
            session_id
                .or(Some("caller"))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Answer {
                leg_id: LegId::new("caller"),
            })
        }

        RwiCommandPayload::Reject { call_id: _, reason } => {
            session_id
                .or(Some("caller"))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Reject {
                leg_id: LegId::new("caller"),
                reason,
            })
        }

        RwiCommandPayload::Ring { call_id: _ } => {
            session_id
                .or(Some("caller"))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Ring {
                leg_id: LegId::new("caller"),
                ringback: None,
            })
        }

        RwiCommandPayload::Hangup {
            call_id: _,
            reason,
            code,
        } => {
            // Hangup doesn't need session_id validation since it cascades to all legs
            let cdr_reason = parse_hangup_reason(reason.as_deref());
            Ok(CallCommand::Hangup(
                HangupCommand::local("rwi", cdr_reason, code).with_cascade(HangupCascade::All),
            ))
        }

        // ========================================================================
        // Bridging
        // ========================================================================
        RwiCommandPayload::Bridge { leg_a, leg_b } => Ok(CallCommand::Bridge {
            leg_a: LegId::new(leg_a),
            leg_b: LegId::new(leg_b),
            mode: P2PMode::Audio,
        }),

        RwiCommandPayload::Unbridge { call_id } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Unbridge {
                leg_id: LegId::new(sid),
            })
        }

        // ========================================================================
        // Transfer
        // ========================================================================
        RwiCommandPayload::Transfer { call_id, target } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Transfer {
                leg_id: LegId::new(sid),
                target,
                attended: false,
            })
        }

        RwiCommandPayload::TransferReplace { call_id, target } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Transfer {
                leg_id: LegId::new(sid),
                target,
                attended: true,
            })
        }

        RwiCommandPayload::TransferAttended {
            call_id,
            target,
            timeout_secs: _,
        } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Transfer {
                leg_id: LegId::new(sid),
                target,
                attended: true,
            })
        }

        RwiCommandPayload::TransferComplete {
            call_id: _,
            consultation_call_id,
        } => Ok(CallCommand::TransferComplete {
            consult_leg: LegId::new(consultation_call_id),
        }),

        RwiCommandPayload::TransferCancel {
            consultation_call_id,
        } => Ok(CallCommand::TransferCancel {
            consult_leg: LegId::new(consultation_call_id),
        }),

        // ========================================================================
        // Hold
        // ========================================================================
        RwiCommandPayload::CallHold { call_id, music } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Hold {
                leg_id: LegId::new(sid),
                music: music.map(MediaSource::file),
            })
        }

        RwiCommandPayload::CallUnhold { call_id } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Unhold {
                leg_id: LegId::new(sid),
            })
        }

        // ========================================================================
        // Media Operations
        // ========================================================================
        // MediaPlay/MediaStop are handled by the legacy processor path which
        // emits media_play_started / media_play_finished RWI events and
        // resolves audio file paths. Letting the unified path handle them
        // would skip event emission and lose loop_playback.
        RwiCommandPayload::MediaPlay(_) => {
            return Err(AdapterError::NotSupported(
                "media.play handled by legacy processor for event emission".to_string(),
            )
            .into());
        }
        RwiCommandPayload::MediaStop { .. } => {
            return Err(AdapterError::NotSupported(
                "media.stop handled by legacy processor for event emission".to_string(),
            )
            .into());
        }

        // ========================================================================
        // DTMF
        // ========================================================================
        RwiCommandPayload::CallSendDtmf { leg_id, digits, .. } => Ok(CallCommand::SendDtmf {
            leg_id: leg_id.map(LegId::new).unwrap_or(LegId::from("caller")),
            digits,
        }),

        RwiCommandPayload::SetRingbackSource {
            target_call_id,
            source_call_id: _,
        } => {
            // This is a complex command that sets ringback from another call
            // For now, treat as a ring command with passthrough
            Ok(CallCommand::Ring {
                leg_id: LegId::new(target_call_id),
                ringback: Some(RingbackPolicy::PassThrough),
            })
        }

        // ========================================================================
        // Recording
        // ========================================================================
        RwiCommandPayload::RecordStart(req) => Ok(CallCommand::StartRecording {
            config: RecordConfig {
                path: req.storage.path,
                max_duration_secs: req.max_duration_secs,
                beep: req.beep.unwrap_or(false),
                format: None, // RWI doesn't have a format field in RecordStartRequest
                channels: None,
                mono_caller_only: None,
            },
        }),

        RwiCommandPayload::RecordPause { call_id: _ } => Ok(CallCommand::PauseRecording),

        RwiCommandPayload::RecordResume { call_id: _ } => Ok(CallCommand::ResumeRecording),

        RwiCommandPayload::RecordStop { call_id: _ } => Ok(CallCommand::StopRecording),

        // ========================================================================
        // Supervisor Operations
        // ========================================================================
        RwiCommandPayload::SupervisorListen {
            supervisor_call_id,
            target_call_id: _,
        } => Ok(CallCommand::SupervisorListen {
            supervisor_leg: LegId::new(supervisor_call_id.clone()),
            target_leg: LegId::new("callee"),
            supervisor_session_id: Some(supervisor_call_id),
        }),

        RwiCommandPayload::SupervisorWhisper {
            supervisor_call_id,
            target_call_id,
            agent_leg: _,
        } => Ok(CallCommand::SupervisorWhisper {
            supervisor_leg: LegId::new(supervisor_call_id.clone()),
            target_leg: LegId::new(target_call_id),
            supervisor_session_id: Some(supervisor_call_id),
        }),

        RwiCommandPayload::SupervisorBarge {
            supervisor_call_id,
            target_call_id,
            agent_leg: _,
        } => Ok(CallCommand::SupervisorBarge {
            supervisor_leg: LegId::new(supervisor_call_id.clone()),
            target_leg: LegId::new(target_call_id),
            supervisor_session_id: Some(supervisor_call_id),
        }),

        RwiCommandPayload::SupervisorTakeover {
            supervisor_call_id,
            target_call_id: _,
        } => Ok(CallCommand::SupervisorTakeover {
            supervisor_leg: LegId::new("callee"),
            target_leg: LegId::new("callee"),
            supervisor_session_id: Some(supervisor_call_id),
        }),

        RwiCommandPayload::SupervisorStop {
            supervisor_call_id,
            target_call_id: _,
        } => Ok(CallCommand::SupervisorStop {
            supervisor_leg: LegId::new(supervisor_call_id),
        }),

        // ========================================================================
        // Queue Operations
        // ========================================================================
        RwiCommandPayload::QueueHold { call_id } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Hold {
                leg_id: LegId::new(sid),
                music: None,
            })
        }

        RwiCommandPayload::QueueUnhold { call_id } => {
            let sid = session_id
                .or(Some(&call_id))
                .ok_or(AdapterError::MissingField("session_id or call_id"))?;
            Ok(CallCommand::Unhold {
                leg_id: LegId::new(sid),
            })
        }

        RwiCommandPayload::QueueEnqueue(_)
        | RwiCommandPayload::QueueDequeue { .. }
        | RwiCommandPayload::QueueSetPriority { .. }
        | RwiCommandPayload::QueueAssignAgent { .. }
        | RwiCommandPayload::QueueRequeue { .. } => {
            // These are queue management commands, not session commands
            Err(AdapterError::NotSupported("queue management command".to_string()).into())
        }

        // ========================================================================
        // Conference Operations (handled at processor level via ConferenceManager)
        // ========================================================================
        RwiCommandPayload::ConferenceCreate(_)
        | RwiCommandPayload::ConferenceAdd { .. }
        | RwiCommandPayload::ConferenceRemove { .. }
        | RwiCommandPayload::ConferenceMute { .. }
        | RwiCommandPayload::ConferenceUnmute { .. }
        | RwiCommandPayload::ConferenceDestroy { .. }
        | RwiCommandPayload::ConferenceMerge { .. }
        | RwiCommandPayload::ConferenceSeatReplace { .. } => Err(AdapterError::NotSupported(
            "conference command requires processor-level handling".to_string(),
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_answer_conversion() {
        let payload = RwiCommandPayload::Answer {
            call_id: "call-123".to_string(),
        };
        let cmd = rwi_to_call_command(payload, None).unwrap();
        assert!(matches!(cmd, CallCommand::Answer { leg_id: _ }));
    }

    #[test]
    fn test_hangup_conversion() {
        let payload = RwiCommandPayload::Hangup {
            call_id: "call-123".to_string(),
            reason: Some("normal_clearing".to_string()),
            code: Some(200),
        };
        let cmd = rwi_to_call_command(payload, None).unwrap();
        if let CallCommand::Hangup(hangup_cmd) = cmd {
            assert_eq!(hangup_cmd.code, Some(200));
        } else {
            panic!("Expected Hangup command");
        }
    }

    #[test]
    fn test_bridge_conversion() {
        let payload = RwiCommandPayload::Bridge {
            leg_a: "leg-a".to_string(),
            leg_b: "leg-b".to_string(),
        };
        let cmd = rwi_to_call_command(payload, None).unwrap();
        if let CallCommand::Bridge { leg_a, leg_b, .. } = cmd {
            assert_eq!(leg_a.as_str(), "leg-a");
            assert_eq!(leg_b.as_str(), "leg-b");
        } else {
            panic!("Expected Bridge command");
        }
    }

    #[test]
    fn test_hold_conversion() {
        let payload = RwiCommandPayload::CallHold {
            call_id: "call-123".to_string(),
            music: Some("music.wav".to_string()),
        };
        let cmd = rwi_to_call_command(payload, None).unwrap();
        if let CallCommand::Hold { leg_id, music } = cmd {
            assert_eq!(leg_id.as_str(), "call-123");
            assert!(matches!(music, Some(MediaSource::File { .. })));
        } else {
            panic!("Expected Hold command");
        }
    }

    #[test]
    fn test_supervisor_listen_conversion() {
        let payload = RwiCommandPayload::SupervisorListen {
            supervisor_call_id: "sup-1".to_string(),
            target_call_id: "target-1".to_string(),
        };
        let cmd = rwi_to_call_command(payload, None).unwrap();
        if let CallCommand::SupervisorListen {
            supervisor_leg,
            target_leg,
            supervisor_session_id,
        } = cmd
        {
            assert_eq!(supervisor_leg.as_str(), "sup-1");
            assert_eq!(target_leg.as_str(), "callee");
            assert_eq!(supervisor_session_id.as_deref(), Some("sup-1"));
        } else {
            panic!("Expected SupervisorListen command");
        }
    }

    #[test]
    fn test_unsupported_command() {
        let payload = RwiCommandPayload::Subscribe {
            contexts: vec!["all".to_string()],
            events: None,
        };
        let result = rwi_to_call_command(payload, None);
        assert!(result.is_err());
    }

}
