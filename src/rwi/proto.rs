use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const RWI_VERSION: &str = "1.0";

/// Common call context flattened into all call-scoped RWI events.
/// All fields are Option — when None they are omitted from JSON.
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
// CallMeta and CallMetaStore — legacy, kept for sipflow_upload backward compat
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
    async fn call_meta_store_insert_and_get() {
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

        let meta = store.get("call-1").await.expect("meta must exist");
        assert_eq!(meta.caller.as_deref(), Some("1001"));
        assert_eq!(meta.callee.as_deref(), Some("1002"));
        assert_eq!(meta.caller_name.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn eventcallcontext_from_call_meta() {
        let meta = CallMeta {
            caller: Some("2001".to_string()),
            callee: Some("2002".to_string()),
            ..Default::default()
        };
        let ctx = EventCallContext::from(meta);
        assert_eq!(ctx.caller.as_deref(), Some("2001"));
        assert_eq!(ctx.callee.as_deref(), Some("2002"));
    }
}
