//! In-memory session registry — for single-node or small deployments where no
//! shared database is available.
//!
//! Owns a local `DashMap` of active sessions plus a SWEA sweeper task that
//! reclaims rows whose `last_update` is older than the TTL.  All read/write
//! operations are O(1) hash lookups with zero network I/O.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::task::JoinHandle;

use super::{RegistryError, SessionInfo, SessionRegistry, SessionRegistryRef};
use crate::utils;

struct RegistryEntry {
    info: SessionInfo,
    last_update: Instant,
}

/// In-memory [`SessionRegistry`] backend.
pub struct MemorySessionRegistry {
    sessions: Arc<DashMap<String, RegistryEntry>>,
    ttl: Duration,
    sweeper_cancel: tokio_util::sync::CancellationToken,
    sweeper_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl MemorySessionRegistry {
    /// Create a registry and start its SWEA sweeper background task.
    pub fn new(node_id: impl Into<String>, ttl: Duration) -> Arc<Self> {
        let _ = node_id.into(); // reserved: nodes in a memory registry are per-instance
        let reg = Arc::new(Self {
            sessions: Arc::new(DashMap::new()),
            ttl,
            sweeper_cancel: tokio_util::sync::CancellationToken::new(),
            sweeper_handle: std::sync::Mutex::new(None),
        });
        reg.clone().start_sweeper();
        reg
    }

    /// Upcast to the trait object for injection into consumers.
    pub fn into_ref(self: Arc<Self>) -> SessionRegistryRef {
        self
    }

    /// Launch the background sweeper (runs every 60s).
    fn start_sweeper(self: &Arc<Self>) {
        let this = self.clone();
        let cancel = self.sweeper_cancel.clone();
        let handle = utils::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await; // skip immediate tick
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => this.sweep(),
                }
            }
        });
        *self.sweeper_handle.lock().expect("sweeper mutex") = Some(handle);
    }

    /// Remove all entries whose last update is older than TTL.
    /// Public so the SWEA behaviour is directly testable.
    pub fn sweep(&self) {
        let cutoff = Instant::now() - self.ttl;
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|e| e.last_update < cutoff)
            .map(|e| e.key().clone())
            .collect();
        for call_id in expired {
            self.sessions.remove(&call_id);
        }
    }

    /// Test helper: number of rows owned by `node_id` touched within `window`.
    pub async fn last_heartbeat_within(&self, node_id: &str, window: Duration) -> usize {
        let now = Instant::now();
        self.sessions
            .iter()
            .filter(|e| e.info.node_id == node_id && now.duration_since(e.last_update) <= window)
            .count()
    }
}

#[async_trait]
impl SessionRegistry for MemorySessionRegistry {
    async fn register(&self, info: &SessionInfo) -> Result<(), RegistryError> {
        self.sessions.insert(
            info.call_id.clone(),
            RegistryEntry {
                info: info.clone(),
                last_update: Instant::now(),
            },
        );
        Ok(())
    }

    async fn unregister(&self, call_id: &str) -> Result<(), RegistryError> {
        self.sessions.remove(call_id);
        Ok(())
    }

    async fn heartbeat_node(&self, node_id: &str) -> Result<(), RegistryError> {
        let now = Instant::now();
        // Single pass over the map — no per-session task/await overhead.
        for mut entry in self.sessions.iter_mut() {
            if entry.info.node_id == node_id {
                entry.last_update = now;
            }
        }
        Ok(())
    }

    async fn lookup_owner(&self, call_id: &str) -> Option<String> {
        self.sessions.get(call_id).map(|e| e.info.node_id.clone())
    }

    async fn lookup(&self, call_id: &str) -> Option<SessionInfo> {
        self.sessions.get(call_id).map(|e| e.info.clone())
    }

    async fn list_all(&self, limit: usize) -> Vec<SessionInfo> {
        let mut entries: Vec<SessionInfo> = self.sessions.iter().map(|e| e.info.clone()).collect();
        entries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        entries.truncate(limit);
        entries
    }

    async fn list_by_node(&self, node_id: &str) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|e| e.info.node_id == node_id)
            .map(|e| e.key().clone())
            .collect()
    }

    async fn active_count(&self) -> usize {
        self.sessions.len()
    }

    async fn health_check(&self) -> Result<(), RegistryError> {
        Ok(())
    }
}

impl Drop for MemorySessionRegistry {
    fn drop(&mut self) {
        self.sweeper_cancel.cancel();
        if let Ok(mut guard) = self.sweeper_handle.lock() {
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(ttl: Duration) -> Arc<MemorySessionRegistry> {
        MemorySessionRegistry::new("node-1", ttl)
    }

    async fn seed(registry: &Arc<MemorySessionRegistry>, call: &str, node: &str) {
        registry
            .register(&SessionInfo::new(call, node))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_lookup_list() {
        let r = reg(Duration::from_secs(3600));
        seed(&r, "a", "node-1").await;
        seed(&r, "b", "node-2").await;

        assert_eq!(r.active_count().await, 2);
        assert_eq!(r.lookup_owner("a").await.as_deref(), Some("node-1"));
        assert!(r.lookup_owner("nope").await.is_none());
        assert_eq!(r.list_by_node("node-2").await, vec!["b".to_string()]);
        let all = r.list_all(10).await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregister_removes() {
        let r = reg(Duration::from_secs(3600));
        seed(&r, "a", "node-1").await;
        r.unregister("a").await.unwrap();
        assert_eq!(r.active_count().await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_node_only_touches_own() {
        let r = reg(Duration::from_secs(3600));
        seed(&r, "own1", "node-1").await;
        seed(&r, "own2", "node-1").await;
        seed(&r, "other", "node-2").await;

        tokio::time::sleep(Duration::from_millis(20)).await; // age node-1 rows
        r.heartbeat_node("node-1").await.unwrap();

        assert_eq!(
            r.last_heartbeat_within("node-1", Duration::from_millis(5))
                .await,
            2
        );
        assert_eq!(
            r.last_heartbeat_within("node-2", Duration::from_millis(5))
                .await,
            0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweeper_reclaims_expired() {
        let r = reg(Duration::from_millis(50));
        seed(&r, "stale", "node-1").await;

        // Age the stale row past TTL, then sweep.
        tokio::time::sleep(Duration::from_millis(80)).await;
        r.sweep();
        assert!(r.lookup_owner("stale").await.is_none());
        assert_eq!(r.active_count().await, 0);

        // A fresh row survives the same sweep.
        seed(&r, "fresh", "node-1").await;
        r.sweep();
        assert_eq!(r.active_count().await, 1);
        assert_eq!(r.lookup_owner("fresh").await.as_deref(), Some("node-1"));
    }
}
