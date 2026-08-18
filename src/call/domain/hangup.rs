//! Hangup types - termination semantics for sessions and legs

use crate::callrecord::CallRecordHangupReason;
use serde::{Deserialize, Serialize};

use super::LegId;

/// How hangup cascades to other legs in the session
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum HangupCascade {
    /// Hangup all legs in the session
    #[default]
    All,
    /// Only hangup the specified leg, leave others intact
    None,
    /// Hangup all legs except the specified ones
    AllExcept(Vec<LegId>),
    /// Hangup the "other" leg in a point-to-point bridge
    Other,
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
    },
    /// System initiated the hangup (timeout, error, etc.)
    System {
        /// System reason for hangup
        reason: SystemHangupReason,
        /// Additional details
        details: Option<String>,
    },
}

/// System-level reasons for hangup
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemHangupReason {
    /// Internal error
    InternalError,
}

impl std::fmt::Display for SystemHangupReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SystemHangupReason::InternalError => write!(f, "internal_error"),
        }
    }
}

/// Which leg of a bridged call caused an RTP inactivity timeout. This lets the
/// CDR / call trace attribute a teardown to the caller or the callee side even
/// though both legs share the `RtpTimeout` hangup reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RtpTimeoutSide {
    /// The caller (LegSide::A) stopped sending RTP.
    Caller,
    /// The callee (LegSide::B) stopped sending RTP.
    Callee,
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
    /// When `reason == RtpTimeout`, which side of the bridge caused it.
    /// Only set for the RTP-inactivity watchdog; `None` otherwise.
    pub rtp_timeout_side: Option<RtpTimeoutSide>,
}

impl HangupCommand {
    /// Create a simple hangup command for all legs
    pub fn all(reason: Option<CallRecordHangupReason>, code: Option<u16>) -> Self {
        Self {
            leg_id: None,
            cascade: HangupCascade::All,
            initiator: HangupInitiator::Local {
                source: "unknown".to_string(),
            },
            reason,
            code,
            rtp_timeout_side: None,
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
            },
            reason,
            code,
            rtp_timeout_side: None,
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
            rtp_timeout_side: None,
        }
    }

    /// Set the cascade mode
    pub fn with_cascade(mut self, cascade: HangupCascade) -> Self {
        self.cascade = cascade;
        self
    }

    /// Attribute the hangup to the RTP-inactivity watchdog and record which
    /// side of the bridge stopped sending RTP.
    pub fn with_rtp_timeout_side(mut self, side: RtpTimeoutSide) -> Self {
        self.reason = Some(CallRecordHangupReason::RtpTimeout);
        self.rtp_timeout_side = Some(side);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hangup_command_all() {
        let cmd = HangupCommand::all(Some(CallRecordHangupReason::BySystem), Some(200));
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
}
