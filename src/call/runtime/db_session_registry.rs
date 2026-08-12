//! PostgreSQL/MySQL-backed session registry — the cluster default when all
//! nodes share one database.
//!
//! Write amplification is minimal by design:
//!
//! - `register` / `unregister` are single-row writes, once per call.
//! - `heartbeat_node` is **one** bulk `UPDATE ... WHERE node_id = $self`
//!   executed by the single [`NodeHeartbeat`] task — never per-session.
//! - SWEA sweeper runs one `DELETE WHERE last_updated_at < cutoff` per minute,
//!   using the `idx_cluster_sessions_updated` index.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use tokio::task::JoinHandle;

use super::{RegistryError, SessionInfo, SessionRegistry, SessionRegistryRef};
use crate::models::cluster_session::{ActiveModel as SessionActiveModel, Column, Entity, Model};
use crate::utils;

/// SWEA sweeper cadence.
const SWEEPER_INTERVAL: Duration = Duration::from_secs(60);

/// [`SessionRegistry`] backed by the shared cluster database.
pub struct DbSessionRegistry {
    db: DatabaseConnection,
    ttl: Duration,
    sweeper_cancel: tokio_util::sync::CancellationToken,
    sweeper_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl DbSessionRegistry {
    /// Connect to the shared DB and start the SWEA sweeper task.
    pub fn new(db: DatabaseConnection, ttl: Duration) -> Arc<Self> {
        let reg = Arc::new(Self {
            db,
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

    fn start_sweeper(self: &Arc<Self>) {
        let this = self.clone();
        let cancel = self.sweeper_cancel.clone();
        let handle = utils::spawn(async move {
            let mut interval = tokio::time::interval(SWEEPER_INTERVAL);
            interval.tick().await; // skip immediate tick
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(e) = this.sweep().await {
                            tracing::error!("session registry sweeper error: {}", e);
                        }
                    }
                }
            }
        });
        *self.sweeper_handle.lock().expect("sweeper mutex") = Some(handle);
    }

    /// Delete rows whose `last_updated_at` is older than TTL (crash recovery).
    async fn sweep(&self) -> Result<(), sea_orm::DbErr> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::from_std(self.ttl).unwrap_or(chrono::Duration::hours(1));
        Entity::delete_many()
            .filter(Column::LastUpdatedAt.lt(cutoff))
            .exec(&self.db)
            .await?;
        Ok(())
    }
}

fn from_model(m: Model) -> SessionInfo {
    SessionInfo {
        call_id: m.call_id,
        node_id: m.node_id,
        caller: m.caller,
        callee: m.callee,
        direction: m.direction,
        started_at: m.started_at,
    }
}

#[async_trait]
impl SessionRegistry for DbSessionRegistry {
    async fn register(&self, info: &SessionInfo) -> Result<(), RegistryError> {
        let model = SessionActiveModel {
            call_id: Set(info.call_id.clone()),
            node_id: Set(info.node_id.clone()),
            caller: Set(info.caller.clone()),
            callee: Set(info.callee.clone()),
            direction: Set(info.direction.clone()),
            started_at: Set(info.started_at),
            last_updated_at: Set(chrono::Utc::now()),
        };
        Entity::insert(model)
            .on_conflict(
                OnConflict::column(Column::CallId)
                    .update_columns([
                        Column::NodeId,
                        Column::Caller,
                        Column::Callee,
                        Column::Direction,
                        Column::LastUpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(|e| RegistryError::Unavailable(e.to_string()))?;
        Ok(())
    }

    async fn unregister(&self, call_id: &str) -> Result<(), RegistryError> {
        Entity::delete_by_id(call_id)
            .exec(&self.db)
            .await
            .map_err(|e| RegistryError::Unavailable(e.to_string()))?;
        Ok(())
    }

    async fn heartbeat_node(&self, node_id: &str) -> Result<(), RegistryError> {
        // Refresh only rows that are due — avoids write amplification.
        let stale_before =
            chrono::Utc::now() - chrono::Duration::seconds(SWEEPER_INTERVAL.as_secs() as i64);
        Entity::update_many()
            .col_expr(Column::LastUpdatedAt, Expr::current_timestamp().into())
            .filter(Column::NodeId.eq(node_id))
            .filter(Column::LastUpdatedAt.lt(stale_before))
            .exec(&self.db)
            .await
            .map_err(|e| RegistryError::Unavailable(e.to_string()))?;
        Ok(())
    }

    async fn lookup_owner(&self, call_id: &str) -> Option<String> {
        Entity::find()
            .select_only()
            .column(Column::NodeId)
            .filter(Column::CallId.eq(call_id))
            .into_tuple::<(String,)>()
            .one(&self.db)
            .await
            .ok()
            .flatten()
            .map(|(node_id,)| node_id)
    }

    async fn lookup(&self, call_id: &str) -> Option<SessionInfo> {
        Entity::find_by_id(call_id)
            .one(&self.db)
            .await
            .ok()
            .flatten()
            .map(from_model)
    }

    async fn list_all(&self, limit: usize) -> Vec<SessionInfo> {
        Entity::find()
            .order_by_desc(Column::StartedAt)
            .limit(limit as u64)
            .all(&self.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(from_model)
            .collect()
    }

    async fn list_by_node(&self, node_id: &str) -> Vec<String> {
        Entity::find()
            .select_only()
            .column(Column::CallId)
            .filter(Column::NodeId.eq(node_id))
            .into_tuple::<(String,)>()
            .all(&self.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(call_id,)| call_id)
            .collect()
    }

    async fn active_count(&self) -> usize {
        Entity::find().count(&self.db).await.unwrap_or(0) as usize
    }

    async fn health_check(&self) -> Result<(), RegistryError> {
        self.db
            .ping()
            .await
            .map_err(|e| RegistryError::Unavailable(e.to_string()))
    }
}

impl Drop for DbSessionRegistry {
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
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    /// File-backed sqlite so all pooled connections share one database.
    async fn test_db() -> DatabaseConnection {
        let path = std::env::temp_dir().join(format!(
            "session-registry-test-{}.sqlite3",
            uuid::Uuid::new_v4()
        ));
        // sqlite won't create the file itself on this setup — mirror the
        // production `prepare_sqlite_database` behaviour.
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .expect("create test db file");
        let url = format!("sqlite://{}", path.display());
        let mut opt = sea_orm::ConnectOptions::new(url);
        opt.max_connections(1);
        let db = sea_orm::Database::connect(opt).await.expect("connect");
        crate::models::cluster_session::Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("migration up");
        // Note: the temp file is intentionally left in place for the duration
        // of the test — deleting it would make the open connection see a
        // "readonly database" on the next write.  The OS temp dir cleans up.
        db
    }

    async fn reg() -> Arc<DbSessionRegistry> {
        DbSessionRegistry::new(test_db().await, Duration::from_secs(3600))
    }

    fn info(call: &str, node: &str) -> SessionInfo {
        let mut i = SessionInfo::new(call, node);
        i.caller = "1001".into();
        i.callee = "1002".into();
        i.direction = "inbound".into();
        i
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_lookup_list() {
        let r = reg().await;
        r.register(&info("a", "node-1")).await.unwrap();
        r.register(&info("b", "node-2")).await.unwrap();

        assert_eq!(r.active_count().await, 2);
        assert_eq!(r.lookup_owner("a").await.as_deref(), Some("node-1"));
        let full = r.lookup("a").await.unwrap();
        assert_eq!(full.caller, "1001");
        assert_eq!(r.list_by_node("node-2").await, vec!["b".to_string()]);
        assert_eq!(r.list_all(10).await.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn register_is_idempotent_upsert() {
        let r = reg().await;
        r.register(&info("a", "node-1")).await.unwrap();
        // Same call id, node ownership changes (session resurrected elsewhere).
        r.register(&info("a", "node-2")).await.unwrap();
        assert_eq!(r.active_count().await, 1);
        assert_eq!(r.lookup_owner("a").await.as_deref(), Some("node-2"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregister_removes() {
        let r = reg().await;
        r.register(&info("a", "node-1")).await.unwrap();
        r.unregister("a").await.unwrap();
        assert_eq!(r.active_count().await, 0);
        assert!(r.lookup_owner("a").await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_node_bulk_refresh() {
        let r = reg().await;
        r.register(&info("own1", "node-1")).await.unwrap();
        r.register(&info("own2", "node-1")).await.unwrap();
        r.register(&info("other", "node-2")).await.unwrap();

        r.heartbeat_node("node-1").await.unwrap();
        // Both node-1 rows exist; node-2 row untouched but still present.
        assert_eq!(r.active_count().await, 3);
        assert_eq!(r.list_by_node("node-1").await.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_check_ok() {
        let r = reg().await;
        assert!(r.health_check().await.is_ok());
    }
}
