//! Session Registry Runtime
//!
//! This module provides the `SessionRegistry` trait that abstracts the
//! active call registry operations, allowing unified session management.

use super::session_runtime::SessionId;

/// Active call view for registry
#[derive(Debug, Clone)]
pub struct ActiveCallView {
    /// Session ID
    pub session_id: SessionId,
    /// Caller identity
    pub caller: Option<String>,
    /// Callee identity
    pub callee: Option<String>,
    /// Call direction
    pub direction: CallDirection,
    /// Call status
    pub status: CallStatus,
    /// Timestamp when call started
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Timestamp when call was answered (if applicable)
    pub answered_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Call direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    Inbound,
    Outbound,
}

impl std::fmt::Display for CallDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallDirection::Inbound => write!(f, "inbound"),
            CallDirection::Outbound => write!(f, "outbound"),
        }
    }
}

/// Call status in registry
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    Ringing,
    Talking,
    Hold,
    Ended,
}

impl std::fmt::Display for CallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallStatus::Ringing => write!(f, "ringing"),
            CallStatus::Talking => write!(f, "talking"),
            CallStatus::Hold => write!(f, "hold"),
            CallStatus::Ended => write!(f, "ended"),
        }
    }
}

/// Session registry trait
///
/// This trait abstracts the active call registry operations,
/// providing a clean interface for session management.
/// For Phase D, this will be integrated with the actual ActiveProxyCallRegistry.
pub trait SessionRegistry: Send + Sync {
    /// Upsert a session entry
    fn upsert(&self, entry: ActiveCallView);

    /// Remove a session by ID
    fn remove(&self, session_id: &str);

    /// Check if a session exists
    fn contains(&self, session_id: &str) -> bool;

    /// Get the number of active sessions
    fn len(&self) -> usize;

    /// Get all active session IDs
    fn session_ids(&self) -> Vec<String>;
}

/// Adapter to wrap existing ActiveProxyCallRegistry
///
/// This adapter bridges the gap between the existing registry
/// and the new SessionRegistry trait during migration.
pub struct RegistryAdapter {
    registry: std::sync::Arc<crate::proxy::active_call_registry::ActiveProxyCallRegistry>,
}

impl RegistryAdapter {
    /// Create a new adapter wrapping the given registry
    pub fn new(registry: std::sync::Arc<crate::proxy::active_call_registry::ActiveProxyCallRegistry>) -> Self {
        Self { registry }
    }

    /// Get the underlying registry reference
    pub fn inner(&self) -> &std::sync::Arc<crate::proxy::active_call_registry::ActiveProxyCallRegistry> {
        &self.registry
    }
}

impl SessionRegistry for RegistryAdapter {
    fn upsert(&self, entry: ActiveCallView) {
        use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};

        // Convert ActiveCallView to ActiveProxyCallEntry
        let proxy_status = match entry.status {
            CallStatus::Ringing => ActiveProxyCallStatus::Ringing,
            CallStatus::Talking | CallStatus::Hold | CallStatus::Ended => ActiveProxyCallStatus::Talking,
        };

        let proxy_entry = ActiveProxyCallEntry {
            session_id: entry.session_id.to_string(),
            caller: entry.caller,
            callee: entry.callee,
            direction: entry.direction.to_string(),
            started_at: entry.started_at,
            answered_at: entry.answered_at,
            status: proxy_status,
        };

        // Note: We can't call upsert here because that requires a handle
        // For now, we just update the entry without the handle
        self.registry.update(&entry.session_id.to_string(), |e| {
            e.caller = proxy_entry.caller;
            e.callee = proxy_entry.callee;
            e.direction = proxy_entry.direction;
            e.answered_at = proxy_entry.answered_at;
            e.status = proxy_entry.status;
        });
    }

    fn remove(&self, session_id: &str) {
        self.registry.remove(session_id);
    }

    fn contains(&self, session_id: &str) -> bool {
        self.registry.get_handle(session_id).is_some()
    }

    fn len(&self) -> usize {
        self.registry.len()
    }

    fn session_ids(&self) -> Vec<String> {
        self.registry.session_ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_direction_display() {
        assert_eq!(CallDirection::Inbound.to_string(), "inbound");
        assert_eq!(CallDirection::Outbound.to_string(), "outbound");
    }

    #[test]
    fn call_status_display() {
        assert_eq!(CallStatus::Ringing.to_string(), "ringing");
        assert_eq!(CallStatus::Talking.to_string(), "talking");
        assert_eq!(CallStatus::Hold.to_string(), "hold");
        assert_eq!(CallStatus::Ended.to_string(), "ended");
    }

    #[test]
    fn active_call_view_creation() {
        let view = ActiveCallView {
            session_id: SessionId::from("test-123"),
            caller: Some("sip:100@example.com".to_string()),
            callee: Some("sip:101@example.com".to_string()),
            direction: CallDirection::Inbound,
            status: CallStatus::Ringing,
            started_at: chrono::Utc::now(),
            answered_at: None,
        };

        assert_eq!(view.session_id, SessionId::from("test-123"));
        assert_eq!(view.status, CallStatus::Ringing);
    }

    #[test]
    fn registry_adapter_with_registry() {
        use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
        let registry = std::sync::Arc::new(ActiveProxyCallRegistry::new());
        let adapter = RegistryAdapter::new(registry);
        assert_eq!(adapter.len(), 0);
        assert!(adapter.session_ids().is_empty());
    }
}
