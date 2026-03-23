//! Leg domain types - participants in a call session

use serde::{Deserialize, Serialize};

/// Unique identifier for a call leg (participant in a session)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LegId(pub String);

impl LegId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LegId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for LegId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for LegId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<LegId> for String {
    fn from(leg_id: LegId) -> Self {
        leg_id.0
    }
}

/// State of a single leg (participant) in a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegState {
    /// Leg is being initialized (SDP negotiation, etc.)
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

impl Default for LegState {
    fn default() -> Self {
        Self::Initializing
    }
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
    /// Display name (if available)
    pub display_name: Option<String>,
    /// Dialog ID (SIP Call-ID + tags)
    pub dialog_id: Option<String>,
    /// Whether this leg initiated the call
    pub is_initiator: bool,
    /// SDP answer (if available)
    pub answer_sdp: Option<String>,
}

impl Leg {
    pub fn new(id: LegId) -> Self {
        Self {
            id,
            state: LegState::default(),
            endpoint: None,
            display_name: None,
            dialog_id: None,
            is_initiator: false,
            answer_sdp: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    pub fn with_dialog_id(mut self, dialog_id: impl Into<String>) -> Self {
        self.dialog_id = Some(dialog_id.into());
        self
    }

    pub fn as_initiator(mut self) -> Self {
        self.is_initiator = true;
        self
    }

    /// Check if the leg is in an active state (can send/receive media)
    pub fn is_active(&self) -> bool {
        matches!(self.state, LegState::Connected | LegState::EarlyMedia)
    }

    /// Check if the leg can be answered
    pub fn can_answer(&self) -> bool {
        matches!(self.state, LegState::Ringing | LegState::EarlyMedia)
    }

    /// Check if the leg can be placed on hold
    pub fn can_hold(&self) -> bool {
        matches!(self.state, LegState::Connected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leg_id_display() {
        let id = LegId::new("leg-123");
        assert_eq!(id.to_string(), "leg-123");
    }

    #[test]
    fn leg_state_transitions() {
        let mut leg = Leg::new(LegId::new("test"));
        assert_eq!(leg.state, LegState::Initializing);
        assert!(!leg.is_active());

        leg.state = LegState::Ringing;
        assert!(leg.can_answer());
        assert!(!leg.can_hold());

        leg.state = LegState::Connected;
        assert!(leg.is_active());
        assert!(leg.can_hold());
        assert!(!leg.can_answer());
    }
}
