use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Root call identity, constant across the call tree.
///
/// Populated with the session's own call context (`root = self`): the call the
/// session belongs to. Derived (transferred) sessions keep their own context —
/// there is no cross-session root propagation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RootCallInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
}

/// Common call context flattened into all call-scoped RWI events.
/// All fields are Option — when None they are omitted from JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventCallContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
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
    pub root: Option<RootCallInfo>,
}

/// Type alias for RWI event sender (used by the cc addon's agent event fanout).
pub type RwiEventTx = tokio::sync::mpsc::UnboundedSender<RwiEvent>;

pub use crate::rwi::event::RwiEvent;

// ═══════════════════════════════════════════════════════════════════════════════
// CallMeta and CallMetaStore — per-call metadata used by sipflow upload and
// event enrichment.
// ═══════════════════════════════════════════════════════════════════════════════

/// Per-call metadata for enriching events at dispatch time.
#[derive(Debug, Clone, Default)]
pub struct CallMeta {
    /// Root session id — constant across every leg of a logical call
    /// (inbound root Call-ID, generated root for originates, inherited by
    /// transfer/consult children). See `call::uui`.
    pub session_id: Option<String>,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub direction: Option<String>,
    pub trunk: Option<String>,
    pub app_id: Option<String>,
    pub routing_target: Option<String>,
    pub root: Option<RootCallInfo>,
}

impl From<CallMeta> for EventCallContext {
    fn from(m: CallMeta) -> Self {
        EventCallContext {
            session_id: m.session_id,
            caller: m.caller,
            callee: m.callee,
            caller_name: m.caller_name,
            callee_name: m.callee_name,
            direction: m.direction,
            trunk: m.trunk,
            app_id: m.app_id,
            routing_target: m.routing_target,
            root: m.root,
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

        let meta = store.get_sync("call-1").expect("meta must exist");
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

    #[tokio::test]
    async fn eventcallcontext_carries_root() {
        let meta = CallMeta {
            caller: Some("2001".to_string()),
            callee: Some("2002".to_string()),
            caller_name: Some("alice".to_string()),
            callee_name: Some("2002".to_string()),
            root: Some(RootCallInfo {
                caller: Some("2001".to_string()),
                caller_name: Some("alice".to_string()),
                callee: Some("2002".to_string()),
                callee_name: Some("2002".to_string()),
                call_id: Some("call-root-1".to_string()),
                start_time: Some("2026-01-01T00:00:00Z".to_string()),
            }),
            ..Default::default()
        };
        let ctx = EventCallContext::from(meta);
        let root = ctx.root.expect("root must be carried through");
        assert_eq!(root.call_id.as_deref(), Some("call-root-1"));
        assert_eq!(root.caller.as_deref(), Some("2001"));
        assert_eq!(root.start_time.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn root_serializes_as_nested_object_and_omits_none() {
        let root = RootCallInfo {
            caller: Some("2001".to_string()),
            call_id: Some("call-root-1".to_string()),
            ..Default::default()
        };
        let ctx = EventCallContext {
            root: Some(root),
            ..Default::default()
        };
        let json = serde_json::to_value(&ctx).unwrap();
        assert_eq!(json["root"]["caller"], "2001");
        assert_eq!(json["root"]["call_id"], "call-root-1");
        assert!(
            json["root"]["callee_name"].is_null(),
            "None fields omitted or null"
        );

        // root=None must be omitted entirely.
        let ctx = EventCallContext {
            root: None,
            ..Default::default()
        };
        let json = serde_json::to_value(&ctx).unwrap();
        assert!(json.get("root").is_none());
    }
}
