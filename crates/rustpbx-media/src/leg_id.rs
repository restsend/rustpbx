use serde::{Deserialize, Serialize};

/// Unique identifier for a call leg (participant in a session).
///
/// Shared between the media engine and signaling layers — defined here in
/// `media` so every consumer (`call::domain`, `proxy`, `conference_mixer`)
/// uses the same type without circular dependencies.
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
