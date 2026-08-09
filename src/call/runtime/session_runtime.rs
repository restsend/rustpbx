//! Session Runtime Types
//!
//! This module provides shared types for session management:
//! - `SessionId`: Unique session identifier
//! - `BridgeConfig`: Bridge configuration for connecting legs
//!
//! The actual session implementation is in `crate::proxy::proxy_call::sip_session::SipSession`.

use crate::call::domain::LegId;
use serde::Serialize;

/// Unique identifier for a session
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SessionId(pub String);

impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.0
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        SessionId(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_string())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Bridge configuration for connecting legs
#[derive(Debug, Clone, Default, Serialize)]
pub struct BridgeConfig {
    /// Whether the bridge is active
    pub active: bool,
    /// The legs involved in the bridge
    pub legs: Vec<LegId>,
}

impl BridgeConfig {
    pub fn new() -> Self {
        Self {
            active: false,
            legs: Vec::new(),
        }
    }

    /// Create a bridge between two legs
    pub fn bridge(leg_a: LegId, leg_b: LegId) -> Self {
        Self {
            active: true,
            legs: vec![leg_a, leg_b],
        }
    }

    /// Check if a leg is in the bridge
    pub fn contains_leg(&self, leg_id: &LegId) -> bool {
        self.legs.contains(leg_id)
    }

    /// Clear the bridge
    pub fn clear(&mut self) {
        self.active = false;
        self.legs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bridge_config() {
        let mut config = BridgeConfig::new();
        assert!(!config.active);
        assert!(config.legs.is_empty());

        let leg_a = LegId::from("a");
        let leg_b = LegId::from("b");
        config = BridgeConfig::bridge(leg_a.clone(), leg_b.clone());
        assert!(config.active);
        assert!(config.contains_leg(&leg_a));
        assert!(config.contains_leg(&leg_b));

        config.clear();
        assert!(!config.active);
        assert!(config.legs.is_empty());
    }
}
