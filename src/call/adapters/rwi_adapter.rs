//! RWI Command Adapter
//!
//! Converts `RwiCommandPayload` to unified `CallCommand`.

use crate::call::domain::*;
use crate::rwi::session::{MediaSource as RwiMediaSource, RwiCommandPayload};
use crate::callrecord::CallRecordHangupReason;
use anyhow::Result;

use super::AdapterError;

/// Convert RWI MediaSource to domain MediaSource
fn convert_media_source(source: RwiMediaSource) -> Option<MediaSource> {
    source.uri.as_ref().map(|uri| MediaSource::file(uri))
}

/// Convert RWI hangup reason string to CallRecordHangupReason
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
        | RwiCommandPayload::AttachCall { .. }
        | RwiCommandPayload::DetachCall { .. }
        | RwiCommandPayload::SipMessage { .. }
        | RwiCommandPayload::SipNotify { .. }
        | RwiCommandPayload::SipOptionsPing { .. }
        | RwiCommandPayload::SessionResume { .. }
        | RwiCommandPayload::CallResume { .. } => {
            Err(AdapterError::NotSupported("session management command".to_string()).into())
        }

        RwiCommandPayload::Originate(_) => {
            Err(AdapterError::NotSupported("originate requires separate handling".to_string()).into())
        }

        // ========================================================================
        // Basic Call Control
        // ========================================================================
        RwiCommandPayload::Answer { call_id } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Answer {
                leg_id: LegId::new(sid),
            })
        }

        RwiCommandPayload::Reject { call_id, reason } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Reject {
                leg_id: LegId::new(sid),
                reason,
            })
        }

        RwiCommandPayload::Ring { call_id } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Ring {
                leg_id: LegId::new(sid),
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
                HangupCommand::local("rwi", cdr_reason, code)
                    .with_cascade(HangupCascade::All),
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
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Unbridge {
                leg_id: LegId::new(sid),
            })
        }

        // ========================================================================
        // Transfer
        // ========================================================================
        RwiCommandPayload::Transfer { call_id, target } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Transfer {
                leg_id: LegId::new(sid),
                target,
                attended: false,
            })
        }

        RwiCommandPayload::TransferAttended {
            call_id,
            target,
            timeout_secs: _,
        } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
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
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Hold {
                leg_id: LegId::new(sid),
                music: music.map(|m| MediaSource::file(m)),
            })
        }

        RwiCommandPayload::CallUnhold { call_id } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Unhold {
                leg_id: LegId::new(sid),
            })
        }

        // ========================================================================
        // Media Operations
        // ========================================================================
        RwiCommandPayload::MediaPlay(req) => {
            let sid = session_id.or(Some(&req.call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            let source = convert_media_source(req.source).unwrap_or(MediaSource::Silence);
            Ok(CallCommand::Play {
                leg_id: Some(LegId::new(sid)),
                source,
                options: Some(PlayOptions {
                    interrupt_on_dtmf: req.interrupt_on_dtmf,
                    ..Default::default()
                }),
            })
        }

        RwiCommandPayload::MediaStop { call_id } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::StopPlayback {
                leg_id: Some(LegId::new(sid)),
            })
        }

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
            target_call_id,
        } => Ok(CallCommand::SupervisorListen {
            supervisor_leg: LegId::new(supervisor_call_id),
            target_leg: LegId::new(target_call_id),
        }),

        RwiCommandPayload::SupervisorWhisper {
            supervisor_call_id,
            target_call_id,
            agent_leg: _,
        } => Ok(CallCommand::SupervisorWhisper {
            supervisor_leg: LegId::new(supervisor_call_id),
            target_leg: LegId::new(target_call_id),
        }),

        RwiCommandPayload::SupervisorBarge {
            supervisor_call_id,
            target_call_id,
            agent_leg: _,
        } => Ok(CallCommand::SupervisorBarge {
            supervisor_leg: LegId::new(supervisor_call_id),
            target_leg: LegId::new(target_call_id),
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
        RwiCommandPayload::QueueEnqueue(req) => Ok(CallCommand::QueueEnqueue {
            leg_id: LegId::new(req.call_id),
            queue_id: req.queue_id,
            priority: req.priority,
        }),

        RwiCommandPayload::QueueDequeue { call_id } => Ok(CallCommand::QueueDequeue {
            leg_id: LegId::new(call_id),
        }),

        RwiCommandPayload::QueueHold { call_id } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Hold {
                leg_id: LegId::new(sid),
                music: None,
            })
        }

        RwiCommandPayload::QueueUnhold { call_id } => {
            let sid = session_id.or(Some(&call_id)).ok_or_else(|| {
                AdapterError::MissingField("session_id or call_id")
            })?;
            Ok(CallCommand::Unhold {
                leg_id: LegId::new(sid),
            })
        }

        RwiCommandPayload::QueueSetPriority { .. }
        | RwiCommandPayload::QueueAssignAgent { .. }
        | RwiCommandPayload::QueueRequeue { .. } => {
            // These are queue management commands, not session commands
            Err(AdapterError::NotSupported("queue management command".to_string()).into())
        }

        // ========================================================================
        // Conference Operations
        // ========================================================================
        RwiCommandPayload::ConferenceCreate(req) => Ok(CallCommand::ConferenceCreate {
            conf_id: req.conf_id,
            options: ConferenceOptions {
                max_participants: req.max_members,
                record: req.record,
                record_path: None,
            },
        }),

        RwiCommandPayload::ConferenceAdd { conf_id, call_id } => Ok(CallCommand::ConferenceAdd {
            conf_id,
            leg_id: LegId::new(call_id),
        }),

        RwiCommandPayload::ConferenceRemove { conf_id, call_id } => {
            Ok(CallCommand::ConferenceRemove {
                conf_id,
                leg_id: LegId::new(call_id),
            })
        }

        RwiCommandPayload::ConferenceMute { conf_id, call_id } => Ok(CallCommand::ConferenceMute {
            conf_id,
            leg_id: LegId::new(call_id),
        }),

        RwiCommandPayload::ConferenceUnmute { conf_id, call_id } => {
            Ok(CallCommand::ConferenceUnmute {
                conf_id,
                leg_id: LegId::new(call_id),
            })
        }

        RwiCommandPayload::ConferenceDestroy { conf_id } => {
            Ok(CallCommand::ConferenceDestroy { conf_id })
        }

        // ========================================================================
        // Conference Merge (handled at processor level, not session level)
        // ========================================================================
        RwiCommandPayload::ConferenceMerge { .. } => {
            Err(AdapterError::NotSupported("conference merge requires separate handling".to_string()).into())
        }

        // ========================================================================
        // Media Streaming (not directly convertible to CallCommand)
        // ========================================================================
        RwiCommandPayload::MediaStreamStart { .. }
        | RwiCommandPayload::MediaStreamStop { .. }
        | RwiCommandPayload::MediaInjectStart { .. }
        | RwiCommandPayload::MediaInjectStop { .. } => {
            Err(AdapterError::NotSupported("media streaming requires separate handling".to_string()).into())
        }

        // ========================================================================
        // Parallel Originate (handled at processor level)
        // ========================================================================
        RwiCommandPayload::ParallelOriginate { .. } => {
            Err(AdapterError::NotSupported("parallel originate requires processor-level handling".to_string()).into())
        }
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
        assert!(matches!(
            cmd,
            CallCommand::Answer {
                leg_id: _
            }
        ));
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
        } = cmd
        {
            assert_eq!(supervisor_leg.as_str(), "sup-1");
            assert_eq!(target_leg.as_str(), "target-1");
        } else {
            panic!("Expected SupervisorListen command");
        }
    }

    #[test]
    fn test_unsupported_command() {
        let payload = RwiCommandPayload::Subscribe {
            contexts: vec!["all".to_string()],
        };
        let result = rwi_to_call_command(payload, None);
        assert!(result.is_err());
    }
}
