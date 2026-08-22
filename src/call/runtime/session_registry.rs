//! # Session Registry
//!
//! Distributed session-location registry that answers the question
//! *"which cluster node owns call X?"* — the foundation for routing
//! supervisor / console / RWI commands to the node hosting a live session.
//!
//! ## Design
//!
//! - [`SessionRegistry`] is a backend-agnostic trait.  Two real backends are
//!   provided: [`DbSessionRegistry`](super::db_session_registry::DbSessionRegistry)
//!   (shared PostgreSQL/MySQL — the cluster default) and
//!   [`MemorySessionRegistry`](super::memory_session_registry::MemorySessionRegistry).
//!   [`NoopSessionRegistry`] is the single-node / disabled stub.
//!
//! - [`SessionGuard`] binds a session's registry lifecycle to an object via
//!   RAII: `register` at construction, `unregister` on `Drop`.  Cleanup runs
//!   on every exit path (normal, error, panic unwind), and `release()` provides
//!   an explicit async variant for session cleanup paths.
//!
//! - [`NodeHeartbeat`] batch-refreshes `last_updated_at` for **all** sessions
//!   owned by this node with a single update per tick.  There is deliberately
//!   **no per-session heartbeat**, so registry write cost does not scale with
//!   concurrent call count (a 10k-call node costs ~1 batch UPDATE per 30s, not
//!   333 writes/s).
//!
//! - SWEA (Session With Expiry Auto-cleanup): every backend runs a background
//!   sweeper that reclaims rows whose `last_updated_at` is older than the TTL.
//!   This is the safety net for crash-without-Drop (kill -9, power loss).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::utils;

/// Immutable routing snapshot placed in the registry at session birth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// SIP Call-ID or proxy session id — the registry primary key.
    pub call_id: String,
    /// Owning PBX node, e.g. `"10.0.0.2:5060"`.
    pub node_id: String,
    pub caller: String,
    pub callee: String,
    /// `"inbound"` | `"outbound"`.
    pub direction: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl SessionInfo {
    /// Minimal constructor used by callers that fill metadata later.
    pub fn new(call_id: impl Into<String>, node_id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            node_id: node_id.into(),
            caller: String::new(),
            callee: String::new(),
            direction: String::new(),
            started_at: chrono::Utc::now(),
        }
    }

    /// Direction prefix for dialog Call-ID → proxy session id alias rows.
    pub const ALIAS_PREFIX: &'static str = "alias:";

    /// Build a registry row that maps a SIP dialog Call-ID onto a session.
    pub fn dialog_alias(
        dialog_call_id: impl Into<String>,
        session_id: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        let session_id = session_id.into();
        Self {
            call_id: dialog_call_id.into(),
            node_id: node_id.into(),
            caller: String::new(),
            callee: String::new(),
            direction: format!("{}{}", Self::ALIAS_PREFIX, session_id),
            started_at: chrono::Utc::now(),
        }
    }

    /// Resolve the canonical proxy session id (unwrap alias rows).
    pub fn canonical_session_id(&self) -> &str {
        self.direction
            .strip_prefix(Self::ALIAS_PREFIX)
            .unwrap_or(self.call_id.as_str())
    }

    pub fn is_alias(&self) -> bool {
        self.direction.starts_with(Self::ALIAS_PREFIX)
    }
}

/// Look up owner + canonical session id for a CTI call_id or dialog Call-ID.
pub async fn resolve_owner_and_session(
    registry: &SessionRegistryRef,
    call_id: &str,
) -> Option<(String, String)> {
    let info = registry.lookup(call_id).await?;
    Some((info.node_id.clone(), info.canonical_session_id().to_string()))
}

/// Errors surfaced by registry backends.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("registry unavailable: {0}")]
    Unavailable(String),
    #[error("session {0} not found")]
    NotFound(String),
    #[error("serialization error: {0}")]
    Serialize(String),
}

/// Backend-agnostic session location registry.
///
/// Implementors MUST be cheap to clone (`Arc`-wrapped) and safe to share
/// across tasks.  All methods are `async` so DB backends can block on I/O
/// without holding a lock.
#[async_trait]
pub trait SessionRegistry: Send + Sync + 'static {
    /// Upsert a session record.  Called once at session birth and idempotent —
    /// a re-register overwrites stale node ownership (e.g. after a crash the
    /// resurrected session claims the same call id on the new owner node).
    async fn register(&self, info: &SessionInfo) -> Result<(), RegistryError>;

    /// Remove a session record.  Called on normal session end (via
    /// [`SessionGuard`] RAII drop).
    async fn unregister(&self, call_id: &str) -> Result<(), RegistryError>;

    /// Batch-refresh `last_updated_at` for every session owned by `node_id`.
    /// Invoked by [`NodeHeartbeat`] roughly once per heartbeat interval; must
    /// be a single bulk statement, never a per-session loop.
    async fn heartbeat_node(&self, node_id: &str) -> Result<(), RegistryError>;

    /// Which node owns `call_id`?  `None` = not found / already ended.
    async fn lookup_owner(&self, call_id: &str) -> Option<String>;

    /// Full session info for `call_id`.
    async fn lookup(&self, call_id: &str) -> Option<SessionInfo>;

    /// All active sessions, newest first, capped at `limit`.
    async fn list_all(&self, limit: usize) -> Vec<SessionInfo>;

    /// Call ids owned by `node_id`.
    async fn list_by_node(&self, node_id: &str) -> Vec<String>;

    /// Number of active sessions.
    async fn active_count(&self) -> usize;

    /// Backend health probe.
    async fn health_check(&self) -> Result<(), RegistryError>;
}

/// A cheap, Send+Sync, clonable reference to any [`SessionRegistry`].
pub type SessionRegistryRef = Arc<dyn SessionRegistry>;

// ---------------------------------------------------------------------------
// Noop backend — single-node / cluster-disabled
// ---------------------------------------------------------------------------

/// Registry stub used when cluster is not configured.  Every operation is a
/// cheap no-op so callers do not need to branch on backend availability.
pub struct NoopSessionRegistry;

#[async_trait]
impl SessionRegistry for NoopSessionRegistry {
    async fn register(&self, _info: &SessionInfo) -> Result<(), RegistryError> {
        Ok(())
    }
    async fn unregister(&self, _call_id: &str) -> Result<(), RegistryError> {
        Ok(())
    }
    async fn heartbeat_node(&self, _node_id: &str) -> Result<(), RegistryError> {
        Ok(())
    }
    async fn lookup_owner(&self, _call_id: &str) -> Option<String> {
        None
    }
    async fn lookup(&self, _call_id: &str) -> Option<SessionInfo> {
        None
    }
    async fn list_all(&self, _limit: usize) -> Vec<SessionInfo> {
        Vec::new()
    }
    async fn list_by_node(&self, _node_id: &str) -> Vec<String> {
        Vec::new()
    }
    async fn active_count(&self) -> usize {
        0
    }
    async fn health_check(&self) -> Result<(), RegistryError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RAII session guard
// ---------------------------------------------------------------------------

/// RAII guard tying a session's registry lifecycle to an owning object.
///
/// Created once per session in `SipSession::new()`:
///
/// ```ignore
/// self._session_registry_guard = Some(
///     SessionGuard::register(registry.clone(), SessionInfo::new(id, node_id)).await?,
/// );
/// ```
///
/// On `Drop` (any exit path — normal, error, panic unwind) a fire-and-forget
/// `unregister` is issued.  Use [`SessionGuard::release`] when an explicit
/// async unregister is wanted (e.g. in `SipSession::cleanup`).
///
/// There is intentionally NO per-session heartbeat task here — liveness of all
/// sessions owned by a node is maintained by the single [`NodeHeartbeat`].
pub struct SessionGuard {
    call_id: String,
    registry: SessionRegistryRef,
    released: bool,
}

impl SessionGuard {
    /// Register `info` and return a guard that unregisters on drop.
    pub async fn register(
        registry: SessionRegistryRef,
        info: SessionInfo,
    ) -> Result<Self, RegistryError> {
        registry.register(&info).await?;
        Ok(Self {
            call_id: info.call_id,
            registry,
            released: false,
        })
    }

    /// Explicitly unregister (async) and prevent the Drop from double-firing.
    pub async fn release(mut self) -> Result<(), RegistryError> {
        let result = self.registry.unregister(&self.call_id).await;
        self.released = true;
        result
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let registry = self.registry.clone();
        let call_id = self.call_id.clone();
        utils::spawn(async move {
            if let Err(e) = registry.unregister(&call_id).await {
                tracing::warn!(call_id = %call_id, error = %e,
                    "session registry unregister failed (RAII drop)");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Per-node batch heartbeat
// ---------------------------------------------------------------------------

/// One background task per cluster node that keeps that node's sessions alive.
///
/// Every `interval` it issues a **single** [`SessionRegistry::heartbeat_node`]
/// call refreshing all locally-owned rows at once.  The sweeper's TTL is the
/// crash-recovery window: a node that stops heartbeating (killed) has its
/// sessions reclaimed after TTL.
pub struct NodeHeartbeat {
    cancel: tokio_util::sync::CancellationToken,
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl NodeHeartbeat {
    pub fn spawn(registry: SessionRegistryRef, node_id: String, interval: Duration) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let c = cancel.clone();
        let handle = utils::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // skip immediate tick
            loop {
                tokio::select! {
                    _ = c.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(e) = registry.heartbeat_node(&node_id).await {
                            tracing::warn!(node_id = %node_id, error = %e,
                                "session registry node heartbeat failed");
                        }
                    }
                }
            }
        });
        Self {
            cancel,
            handle: std::sync::Mutex::new(Some(handle)),
        }
    }
}

impl Drop for NodeHeartbeat {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_info_new_has_node_and_call() {
        let info = SessionInfo::new("call-1", "10.0.0.2:5060");
        assert_eq!(info.call_id, "call-1");
        assert_eq!(info.node_id, "10.0.0.2:5060");
        assert!(info.direction.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn noop_registry_is_noop() {
        let reg = NoopSessionRegistry;
        let info = SessionInfo::new("call-1", "node-1");
        reg.register(&info).await.unwrap();
        assert!(reg.lookup_owner("call-1").await.is_none());
        assert_eq!(reg.active_count().await, 0);
        assert!(reg.list_all(10).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_guard_register_then_release() {
        // Use a real memory registry so we can observe effects.
        let reg = super::super::memory_session_registry::MemorySessionRegistry::new(
            "node-1",
            Duration::from_secs(3600),
        );
        let guard = SessionGuard::register(reg.clone(), SessionInfo::new("call-1", "node-1"))
            .await
            .unwrap();
        assert_eq!(reg.active_count().await, 1);
        assert_eq!(reg.lookup_owner("call-1").await.as_deref(), Some("node-1"));

        guard.release().await.unwrap();
        assert_eq!(reg.active_count().await, 0);
        assert!(reg.lookup_owner("call-1").await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_guard_drop_unregisters() {
        let reg = super::super::memory_session_registry::MemorySessionRegistry::new(
            "node-1",
            Duration::from_secs(3600),
        );
        {
            let _guard =
                SessionGuard::register(reg.clone(), SessionInfo::new("call-drop", "node-1"))
                    .await
                    .unwrap();
            assert_eq!(reg.active_count().await, 1);
        }
        // Drop is fire-and-forget: give the spawned task a beat to run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(reg.active_count().await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_guard_drop_does_not_double_unregister() {
        let reg = super::super::memory_session_registry::MemorySessionRegistry::new(
            "node-1",
            Duration::from_secs(3600),
        );
        let guard = SessionGuard::register(reg.clone(), SessionInfo::new("call-x", "node-1"))
            .await
            .unwrap();
        // release marks released=true, Drop later is a no-op — no panic/dupe.
        guard.release().await.unwrap();
        assert_eq!(reg.active_count().await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dialog_alias_resolves_to_canonical_session() {
        let reg = super::super::memory_session_registry::MemorySessionRegistry::new(
            "10.0.0.1:5060",
            Duration::from_secs(3600),
        );
        reg.register(&SessionInfo::new("sess-abc", "10.0.0.1:5060"))
            .await
            .unwrap();
        reg.register(&SessionInfo::dialog_alias(
            "dlg-bleg-1",
            "sess-abc",
            "10.0.0.1:5060",
        ))
        .await
        .unwrap();

        let (owner, canonical) = resolve_owner_and_session(&(reg.clone() as SessionRegistryRef), "dlg-bleg-1")
            .await
            .expect("alias should resolve");
        assert_eq!(owner, "10.0.0.1:5060");
        assert_eq!(canonical, "sess-abc");
    }

    #[test]
    fn session_info_alias_helpers() {
        let alias = SessionInfo::dialog_alias("dlg", "sess", "node");
        assert!(alias.is_alias());
        assert_eq!(alias.canonical_session_id(), "sess");
        let plain = SessionInfo::new("sess", "node");
        assert!(!plain.is_alias());
        assert_eq!(plain.canonical_session_id(), "sess");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn node_heartbeat_batch_refreshes_own_sessions() {
        let reg = super::super::memory_session_registry::MemorySessionRegistry::new(
            "node-1",
            Duration::from_secs(3600),
        );
        let _a = SessionGuard::register(reg.clone(), SessionInfo::new("a", "node-1"))
            .await
            .unwrap();
        let _b = SessionGuard::register(reg.clone(), SessionInfo::new("b", "node-1"))
            .await
            .unwrap();
        let _c = SessionGuard::register(reg.clone(), SessionInfo::new("c", "node-2"))
            .await
            .unwrap();

        // Batch heartbeat touches only node-1's rows.
        reg.heartbeat_node("node-1").await.unwrap();
        let refreshed = reg
            .last_heartbeat_within("node-1", Duration::from_secs(5))
            .await;
        assert_eq!(refreshed, 2, "only node-1's own sessions are refreshed");
    }
}
