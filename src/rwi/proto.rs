use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

/// Thread-safe, concurrent in-memory store mapping call_id → CallMeta.
pub struct CallMetaStore {
    store: DashMap<String, CallMeta>,
}

impl CallMetaStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            store: DashMap::new(),
        })
    }

    pub fn insert(&self, call_id: String, meta: CallMeta) {
        self.store.insert(call_id, meta);
    }

    pub fn get(&self, call_id: &str) -> Option<CallMeta> {
        self.store.get(call_id).map(|r| r.clone())
    }

    /// Synchronous lookup (identical to `get` — both are sync with DashMap).
    pub fn get_sync(&self, call_id: &str) -> Option<CallMeta> {
        self.store.get(call_id).map(|r| r.clone())
    }

    pub fn remove(&self, call_id: &str) {
        self.store.remove(call_id);
    }

    /// Current number of entries in the store.
    pub fn len(&self) -> usize {
        self.store.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecordingMetadata {
    pub filename: String,
    pub file_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee_name: Option<String>,
    pub call_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_end_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_time: Option<String>,
    /// Generic metadata bag, populated from `CallDetails.metadata`. Addons
    /// write flat string keys (e.g. `agent_id`, `queue_id`, `tenant_id`)
    /// that the core passes through without naming — external consumers
    /// read what they need.
    #[serde(flatten)]
    pub extra: Option<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn call_meta_store_insert_and_get() {
        let store = CallMetaStore::new();
        store.insert(
            "call-1".to_string(),
            CallMeta {
                caller: Some("1001".to_string()),
                callee: Some("1002".to_string()),
                caller_name: Some("alice".to_string()),
                ..Default::default()
            },
        );

        let meta = store.get("call-1").expect("meta must exist");
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
