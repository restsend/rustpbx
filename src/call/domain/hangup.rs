//! Hangup types - termination semantics for sessions and legs

use crate::callrecord::CallRecordHangupReason;
use serde::{Deserialize, Serialize};

use super::LegId;

/// How hangup cascades to other legs in the session
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HangupCascade {
    /// Hangup all legs in the session
    All,
    /// Only hangup the specified leg, leave others intact
    None,
    /// Hangup all legs except the specified ones
    AllExcept(Vec<LegId>),
    /// Hangup the "other" leg in a point-to-point bridge
    Other,
}

impl Default for HangupCascade {
    fn default() -> Self {
        Self::All
    }
}

/// Who initiated the hangup
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HangupInitiator {
    /// Remote endpoint initiated the hangup (BYE received)
    Remote {
        /// The leg that received the BYE
        leg_id: LegId,
        /// SIP response code
        sip_code: u16,
        /// Optional reason phrase
        reason: Option<String>,
    },
    /// Local system initiated the hangup (via command)
    Local {
        /// Source of the command (RWI, Console, etc.)
        source: String,
        /// Optional command ID for tracking
        command_id: Option<String>,
    },
    /// System initiated the hangup (timeout, error, etc.)
    System {
        /// System reason for hangup
        reason: SystemHangupReason,
        /// Additional details
        details: Option<String>,
    },
    /// Media error caused the hangup
    Media {
        /// The leg with the media error
        leg_id: LegId,
        /// Error description
        error: String,
    },
}

/// System-level reasons for hangup
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemHangupReason {
    /// Call timed out (no answer)
    NoAnswer,
    /// Session expired
    SessionExpired,
    /// Maximum call duration exceeded
    MaxDurationExceeded,
    /// Queue timeout
    QueueTimeout,
    /// No available agent
    NoAgentAvailable,
    /// System shutdown
    SystemShutdown,
    /// Internal error
    InternalError,
    /// Resource limit exceeded
    ResourceLimit,
    /// Rejected by policy
    PolicyRejection,
}

impl std::fmt::Display for SystemHangupReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SystemHangupReason::NoAnswer => write!(f, "no_answer"),
            SystemHangupReason::SessionExpired => write!(f, "session_expired"),
            SystemHangupReason::MaxDurationExceeded => write!(f, "max_duration_exceeded"),
            SystemHangupReason::QueueTimeout => write!(f, "queue_timeout"),
            SystemHangupReason::NoAgentAvailable => write!(f, "no_agent_available"),
            SystemHangupReason::SystemShutdown => write!(f, "system_shutdown"),
            SystemHangupReason::InternalError => write!(f, "internal_error"),
            SystemHangupReason::ResourceLimit => write!(f, "resource_limit"),
            SystemHangupReason::PolicyRejection => write!(f, "policy_rejection"),
        }
    }
}

/// Hangup command with full context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HangupCommand {
    /// Which leg to hangup (None = all legs)
    pub leg_id: Option<LegId>,
    /// How to cascade the hangup
    pub cascade: HangupCascade,
    /// Who initiated the hangup
    pub initiator: HangupInitiator,
    /// Hangup reason for CDR
    pub reason: Option<CallRecordHangupReason>,
    /// SIP response code
    pub code: Option<u16>,
}

impl HangupCommand {
    /// Create a simple hangup command for all legs
    pub fn all(reason: Option<CallRecordHangupReason>, code: Option<u16>) -> Self {
        Self {
            leg_id: None,
            cascade: HangupCascade::All,
            initiator: HangupInitiator::Local {
                source: "unknown".to_string(),
                command_id: None,
            },
            reason,
            code,
        }
    }

    /// Create a hangup command initiated by a local source
    pub fn local(
        source: impl Into<String>,
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
    ) -> Self {
        Self {
            leg_id: None,
            cascade: HangupCascade::All,
            initiator: HangupInitiator::Local {
                source: source.into(),
                command_id: None,
            },
            reason,
            code,
        }
    }

    /// Create a hangup command from remote BYE
    pub fn remote(
        leg_id: LegId,
        sip_code: u16,
        reason: Option<String>,
        cdr_reason: Option<CallRecordHangupReason>,
    ) -> Self {
        Self {
            leg_id: Some(leg_id.clone()),
            cascade: HangupCascade::default(),
            initiator: HangupInitiator::Remote {
                leg_id,
                sip_code,
                reason,
            },
            reason: cdr_reason,
            code: Some(sip_code),
        }
    }

    /// Create a system-initiated hangup
    pub fn system(reason: SystemHangupReason, details: Option<String>) -> Self {
        Self {
            leg_id: None,
            cascade: HangupCascade::All,
            initiator: HangupInitiator::System { reason, details },
            reason: None,
            code: Some(500),
        }
    }

    /// Set the cascade mode
    pub fn with_cascade(mut self, cascade: HangupCascade) -> Self {
        self.cascade = cascade;
        self
    }

    /// Set the command ID for tracking
    pub fn with_command_id(mut self, id: impl Into<String>) -> Self {
        if let HangupInitiator::Local { source, .. } = &mut self.initiator {
            self.initiator = HangupInitiator::Local {
                source: source.clone(),
                command_id: Some(id.into()),
            };
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hangup_command_all() {
        let cmd = HangupCommand::all(
            Some(CallRecordHangupReason::BySystem),
            Some(200),
        );
        assert_eq!(cmd.cascade, HangupCascade::All);
        assert!(cmd.leg_id.is_none());
    }

    #[test]
    fn hangup_command_remote() {
        let cmd = HangupCommand::remote(
            LegId::new("leg-1"),
            486,
            Some("Busy Here".to_string()),
            Some(CallRecordHangupReason::Rejected),
        );
        assert!(cmd.leg_id.is_some());
        assert_eq!(cmd.code, Some(486));

        if let HangupInitiator::Remote { sip_code, .. } = cmd.initiator {
            assert_eq!(sip_code, 486);
        } else {
            panic!("Expected Remote initiator");
        }
    }

    #[test]
    fn system_hangup_reason_display() {
        assert_eq!(
            SystemHangupReason::NoAnswer.to_string(),
            "no_answer"
        );
    }
}
