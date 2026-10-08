//! Leg domain types - participants in a call session

use serde::{Deserialize, Serialize};

/// Re-exported from `media::leg_id` so the entire codebase uses one definition
/// without circular dependencies.
pub use crate::media::LegId;

/// State of a single leg (participant) in a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum LegState {
    /// Leg is being initialized (SDP negotiation, etc.)
    #[default]
    Initializing,
    /// Leg is ringing (180 Ringing sent/received)
    Ringing,
    /// Early media is active (183 Session Progress)
    EarlyMedia,
    /// Leg is connected (200 OK received/sent)
    Connected,
    /// Leg is on hold
    Hold,
    /// Leg is being terminated
    Ending,
    /// Leg has been terminated
    Ended,
}

impl std::fmt::Display for LegState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LegState::Initializing => write!(f, "initializing"),
            LegState::Ringing => write!(f, "ringing"),
            LegState::EarlyMedia => write!(f, "early_media"),
            LegState::Connected => write!(f, "connected"),
            LegState::Hold => write!(f, "hold"),
            LegState::Ending => write!(f, "ending"),
            LegState::Ended => write!(f, "ended"),
        }
    }
}

/// Information about a call leg
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Leg {
    /// Unique identifier for this leg
    pub id: LegId,
    /// Current state of the leg
    pub state: LegState,
    /// SIP URI or endpoint identifier
    pub endpoint: Option<String>,
    /// Canonical agent identity for this participant, independent of session routing.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Originating peer for an explicitly composed dial; not bridge membership.
    #[serde(default)]
    pub source_leg: Option<LegId>,
    /// Declared dial intent carried over from `CallCommand::LegAdd` (`Agent` /
    /// `Consult`). Together with `agent_id` it drives the RWI `leg_role`
    /// attribution on leg-scoped events (see [`Leg::leg_role`]).
    #[serde(default)]
    pub purpose: Option<super::LegPurpose>,
}

impl Leg {
    pub fn new(id: LegId) -> Self {
        Self {
            id,
            state: LegState::default(),
            endpoint: None,
            agent_id: None,
            source_leg: None,
            purpose: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Declared dial intent (queue-dispatched agent / consult), if any.
    pub fn purpose(&self) -> Option<super::LegPurpose> {
        self.purpose
    }

    /// RWI `leg_role` semantics — the single rule event consumers use to tell
    /// agent-leg events apart:
    ///
    /// - `"agent"`: this leg is a CC agent leg — a queue-dispatched agent, a
    ///   consult target that resolved to a registered agent, or the caller of
    ///   an agent-initiated call. Every event carrying this role describes the
    ///   agent leg only.
    /// - `"consult"`: consult leg whose target is not a registered agent
    ///   (external expert / number).
    /// - `"caller"` / `"callee"`: positional fallback for non-CC legs.
    pub fn leg_role(&self) -> &'static str {
        if self.agent_id.is_some() {
            "agent"
        } else if self.purpose == Some(super::LegPurpose::Consult) {
            "consult"
        } else if self.id.as_str() == "caller" {
            "caller"
        } else {
            "callee"
        }
    }

    /// Check if the leg is in an active state (can send/receive media)
    pub fn is_active(&self) -> bool {
        matches!(self.state, LegState::Connected | LegState::EarlyMedia)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leg_state_transitions() {
        let mut leg = Leg::new(LegId::new("test"));
        assert_eq!(leg.state, LegState::Initializing);
        assert!(!leg.is_active());

        leg.state = LegState::Connected;
        assert!(leg.is_active());
    }

    #[test]
    fn leg_role_agent_when_agent_attribution_present() {
        let mut leg = Leg::new(LegId::new("uuid-leg"));
        assert_eq!(leg.leg_role(), "callee");
        leg.agent_id = Some("1001".into());
        assert_eq!(leg.leg_role(), "agent");
    }

    #[test]
    fn leg_role_consult_for_unregistered_consult_target() {
        let mut leg = Leg::new(LegId::new("consult-1"));
        leg.purpose = Some(super::super::LegPurpose::Consult);
        assert_eq!(leg.leg_role(), "consult");
        // A consult target that IS a registered agent carries agent_id —
        // it is an agent leg.
        leg.agent_id = Some("expert".into());
        assert_eq!(leg.leg_role(), "agent");
    }

    #[test]
    fn leg_role_caller_fallback() {
        let leg = Leg::new(LegId::from("caller"));
        assert_eq!(leg.leg_role(), "caller");
    }

    #[test]
    fn leg_purpose_survives_serde_roundtrip() {
        let mut leg = Leg::new(LegId::new("a"));
        leg.purpose = Some(super::super::LegPurpose::Agent);
        let json = serde_json::to_string(&leg).unwrap();
        let back: Leg = serde_json::from_str(&json).unwrap();
        assert_eq!(back.purpose, Some(super::super::LegPurpose::Agent));

        // Older payloads without the field deserialize with `purpose: None`.
        let legacy: Leg = serde_json::from_str(r#"{"id":"b","state":"initializing"}"#).unwrap();
        assert_eq!(legacy.purpose, None);
    }
}
