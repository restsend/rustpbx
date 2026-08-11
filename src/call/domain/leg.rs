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
}

impl Leg {
    pub fn new(id: LegId) -> Self {
        Self {
            id,
            state: LegState::default(),
            endpoint: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
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
}
