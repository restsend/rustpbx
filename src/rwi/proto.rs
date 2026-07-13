use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const RWI_VERSION: &str = "1.0";

/// Common call context flattened into all call-scoped RWI events.
/// All fields are Option — when None they are omitted from JSON.
/// When enriched, gateway populates from `CallMetaStore`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventCallContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiEnvelope<T> {
    #[serde(rename = "rwi")]
    pub version: String,
    #[serde(flatten)]
    pub payload: T,
}

impl<T> RwiEnvelope<T> {
    pub fn new(payload: T) -> Self {
        Self {
            version: RWI_VERSION.to_string(),
            payload,
        }
    }
}

/// Type alias for RWI event sender.
pub type RwiEventTx = tokio::sync::mpsc::UnboundedSender<RwiEvent>;
/// Type alias for RWI event receiver.
pub type RwiEventRx = tokio::sync::mpsc::UnboundedReceiver<RwiEvent>;

pub use crate::rwi::event::RwiEvent;

// ═══════════════════════════════════════════════════════════════════════════════
// CallMeta and CallMetaStore
// ═══════════════════════════════════════════════════════════════════════════════

/// Per-call metadata for enriching events at dispatch time.
#[derive(Debug, Clone, Default)]
pub struct CallMeta {
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub direction: Option<String>,
    pub trunk: Option<String>,
    pub app_id: Option<String>,
    pub routing_target: Option<String>,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
}

impl From<CallMeta> for EventCallContext {
    fn from(m: CallMeta) -> Self {
        EventCallContext {
            caller: m.caller,
            callee: m.callee,
            caller_name: m.caller_name,
            callee_name: m.callee_name,
            direction: m.direction,
            trunk: m.trunk,
            app_id: m.app_id,
            routing_target: m.routing_target,
            agent_id: m.agent_id,
            agent_name: m.agent_name,
        }
    }
}

/// Thread-safe, in-memory store mapping call_id → CallMeta.
pub struct CallMetaStore {
    store: RwLock<HashMap<String, CallMeta>>,
}

impl CallMetaStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            store: RwLock::new(HashMap::new()),
        })
    }

    pub async fn insert(&self, call_id: String, meta: CallMeta) {
        self.store.write().await.insert(call_id, meta);
    }

    pub async fn get(&self, call_id: &str) -> Option<CallMeta> {
        self.store.read().await.get(call_id).cloned()
    }

    /// Synchronous non-blocking lookup.
    pub fn get_sync(&self, call_id: &str) -> Option<CallMeta> {
        self.store.try_read().ok()?.get(call_id).cloned()
    }

    pub async fn remove(&self, call_id: &str) {
        self.store.write().await.remove(call_id);
    }

    /// Update the agent fields on an existing call's metadata.
    ///
    /// Called when a call is assigned to an agent (e.g. on call connected) so
    /// that subsequently emitted events (record_end, recording_metadata_available,
    /// call_hangup, …) are enriched with `agent_id`/`agent_name` via
    /// [`crate::rwi::gateway::RwiGateway::enrich_flat_event`].
    ///
    /// If no entry exists for `call_id` yet (rare race with session creation),
    /// a default [`CallMeta`] is inserted carrying only the agent fields.
    pub async fn update_agent(
        &self,
        call_id: &str,
        agent_id: Option<String>,
        agent_name: Option<String>,
    ) {
        let mut store = self.store.write().await;
        let meta = store.entry(call_id.to_string()).or_default();
        meta.agent_id = agent_id;
        meta.agent_name = agent_name;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecordingMetadata {
    pub filename: String,
    pub unique_id: String,
    pub file_size: u64,
    pub download_url: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub called_phone: Option<String>,
    pub call_type: String,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub call_start_time: Option<String>,
    pub call_end_time: Option<String>,
    pub upload_time: Option<String>,
    pub switch_flag: Option<String>,
    pub process_flag: Option<String>,
    pub root_call_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn update_agent_sets_fields_on_existing_entry() {
        let store = CallMetaStore::new();
        store
            .insert(
                "call-1".to_string(),
                CallMeta {
                    caller: Some("1001".to_string()),
                    callee: Some("1002".to_string()),
                    caller_name: Some("alice".to_string()),
                    ..Default::default()
                },
            )
            .await;

        store
            .update_agent(
                "call-1",
                Some("agent-007".to_string()),
                Some("James Bond".to_string()),
            )
            .await;

        let meta = store.get("call-1").await.expect("meta must exist");
        assert_eq!(meta.agent_id.as_deref(), Some("agent-007"));
        assert_eq!(meta.agent_name.as_deref(), Some("James Bond"));
        // Existing fields must be preserved.
        assert_eq!(meta.caller.as_deref(), Some("1001"));
        assert_eq!(meta.caller_name.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn update_agent_creates_default_entry_when_absent() {
        let store = CallMetaStore::new();
        // No prior insert for "call-2".
        store
            .update_agent("call-2", Some("agent-x".to_string()), None)
            .await;

        let meta = store.get("call-2").await.expect("meta created on the fly");
        assert_eq!(meta.agent_id.as_deref(), Some("agent-x"));
        assert!(meta.agent_name.is_none());
    }

    #[tokio::test]
    async fn update_agent_clears_fields_when_none() {
        let store = CallMetaStore::new();
        store
            .insert(
                "call-3".to_string(),
                CallMeta {
                    agent_id: Some("old".to_string()),
                    agent_name: Some("old name".to_string()),
                    ..Default::default()
                },
            )
            .await;

        store.update_agent("call-3", None, None).await;

        let meta = store.get("call-3").await.unwrap();
        assert!(meta.agent_id.is_none());
        assert!(meta.agent_name.is_none());
    }

    #[tokio::test]
    async fn update_agent_then_eventcallcontext_from_carries_fields() {
        let store = CallMetaStore::new();
        store
            .insert(
                "call-4".to_string(),
                CallMeta {
                    caller: Some("2001".to_string()),
                    ..Default::default()
                },
            )
            .await;
        store
            .update_agent("call-4", Some("agent-9".to_string()), Some("Nine".to_string()))
            .await;

        let meta = store.get("call-4").await.unwrap();
        let ctx = EventCallContext::from(meta);
        assert_eq!(ctx.agent_id.as_deref(), Some("agent-9"));
        assert_eq!(ctx.agent_name.as_deref(), Some("Nine"));
        assert_eq!(ctx.caller.as_deref(), Some("2001"));
    }
}
