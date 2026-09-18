//! PostgreSQL/MySQL-backed session registry — the cluster default when all
//! nodes share one database.
//!
//! Write amplification is minimal by design:
//!
//! - `register` / `unregister` are single-row writes, once per call.
//! - `heartbeat_node` is **one** bulk `UPDATE ... WHERE node_id = $self AND
//!   call_id IN (live ids)` executed by the single [`NodeHeartbeat`] task —
//!   never per-session.  Scoping to live ids means a ghost row (session whose
//!   unregister failed or that never terminated) stops being refreshed and is
//!   reclaimed by the sweeper instead of being kept alive forever.
//! - SWEA sweeper runs one `DELETE WHERE last_updated_at < cutoff` per minute,
//!   using the `idx_cluster_sessions_updated` index, plus an age-based cut
//!   `DELETE WHERE started_at < now - max_age` that bounds any possible ghost
//!   regardless of heartbeat freshness.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
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
    max_age: Duration,
    sweeper_cancel: tokio_util::sync::CancellationToken,
    sweeper_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl DbSessionRegistry {
    /// Connect to the shared DB and start the SWEA sweeper task.
    ///
    /// `ttl` is the heartbeat-freshness window (crash recovery); `max_age` is
    /// the absolute ceiling for any row by `started_at` — the last-resort
    /// bound that keeps ghosts mortal even if a heartbeat bug refreshes them.
    pub fn new(db: DatabaseConnection, ttl: Duration, max_age: Duration) -> Arc<Self> {
        let reg = Arc::new(Self {
            db,
            ttl,
            max_age,
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
                            crate::db_report::report_db_write_failure(
                                "cluster_sessions",
                                "sweep",
                                None,
                                &e,
                            );
                        }
                    }
                }
            }
        });
        *self.sweeper_handle.lock().expect("sweeper mutex") = Some(handle);
    }

    /// Delete stale rows:
    ///
    /// - rows whose `last_updated_at` is older than TTL (crash recovery), and
    /// - rows whose `started_at` is older than `max_age` regardless of
    ///   freshness (ghost bound — e.g. a session that never terminated and
    ///   whose row was still being heartbeat-refreshed).
    pub async fn sweep(&self) -> Result<(), sea_orm::DbErr> {
        let now = chrono::Utc::now();
        let freshness_cutoff =
            now - chrono::Duration::from_std(self.ttl).unwrap_or(chrono::Duration::hours(1));
        Entity::delete_many()
            .filter(Column::LastUpdatedAt.lt(freshness_cutoff))
            .exec(&self.db)
            .await?;

        let age_cutoff =
            now - chrono::Duration::from_std(self.max_age).unwrap_or(chrono::Duration::hours(12));
        Entity::delete_many()
            .filter(Column::StartedAt.lt(age_cutoff))
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
        target_session_id: m.target_session_id,
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
            target_session_id: Set(info.target_session_id.clone()),
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
                        Column::TargetSessionId,
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

    async fn heartbeat_node(
        &self,
        node_id: &str,
        live_call_ids: &[String],
    ) -> Result<(), RegistryError> {
        if live_call_ids.is_empty() {
            return Ok(());
        }
        // Refresh only live rows owned by this node — avoids write
        // amplification AND keeps ghosts (ids not in the list) mortal so the
        // sweeper can reclaim them.  App-clock `Expr::value` keeps the stored
        // format identical to `register`/`unregister` writers (sqlite compares
        // DATETIME TEXT lexicographically; mixing formats breaks ordering) and
        // avoids DB-vs-app clock skew.
        let stale_before =
            chrono::Utc::now() - chrono::Duration::seconds(SWEEPER_INTERVAL.as_secs() as i64);
        Entity::update_many()
            .col_expr(Column::LastUpdatedAt, Expr::value(chrono::Utc::now()))
            .filter(Column::NodeId.eq(node_id))
            .filter(Column::CallId.is_in(live_call_ids.iter().cloned()))
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
        DbSessionRegistry::new(
            test_db().await,
            Duration::from_secs(3600),
            Duration::from_secs(crate::call::runtime::DEFAULT_SESSION_MAX_AGE_SECS),
        )
    }

    /// Short-window registry for sweeper tests: TTL 60s freshness, 300s age
    /// bound — lets tests backdate rows by minutes instead of hours.
    async fn reg_short() -> (Arc<DbSessionRegistry>, DatabaseConnection) {
        let db = test_db().await;
        let reg = DbSessionRegistry::new(
            db.clone(),
            Duration::from_secs(60),
            Duration::from_secs(300),
        );
        (reg, db)
    }

    /// Force a row's `last_updated_at` / `started_at` into the past.
    async fn backdate_row(
        db: &DatabaseConnection,
        call_id: &str,
        last_updated_secs_ago: i64,
        started_secs_ago: i64,
    ) {
        use sea_orm::sea_query::Expr;
        let now = chrono::Utc::now();
        Entity::update_many()
            .col_expr(
                Column::LastUpdatedAt,
                Expr::value(now - chrono::Duration::seconds(last_updated_secs_ago)),
            )
            .col_expr(
                Column::StartedAt,
                Expr::value(now - chrono::Duration::seconds(started_secs_ago)),
            )
            .filter(Column::CallId.eq(call_id))
            .exec(db)
            .await
            .unwrap();
    }

    async fn row_freshness(
        db: &DatabaseConnection,
        call_id: &str,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        Entity::find_by_id(call_id)
            .one(db)
            .await
            .unwrap()
            .map(|m| m.last_updated_at)
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

    /// Incident 2026-09-17 regression: the alias payload used to be stuffed
    /// into the 16-wide `direction` enum column, so every alias insert failed
    /// on strict deployments ("Data too long for column 'direction'") and
    /// cross-node dialog Call-ID resolution silently died. The target id now
    /// lives in its own 200-wide column and must round-trip — including at
    /// full column width (verbatim Call-IDs reach that size) — through
    /// register, lookup, the cluster resolver, and upsert.
    #[tokio::test(flavor = "multi_thread")]
    async fn dialog_alias_persists_target_session_id() {
        let db = test_db().await;
        let r = DbSessionRegistry::new(
            db.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(crate::call::runtime::DEFAULT_SESSION_MAX_AGE_SECS),
        );
        let target = format!("{}-{}", "a".repeat(32), "b".repeat(100)); // 133 chars > 16
        r.register(&SessionInfo::dialog_alias(
            "dlg-call-id",
            target.clone(),
            "node-1",
        ))
        .await
        .unwrap();

        // Raw row shape: direction stays the short enum marker, payload in
        // the dedicated column.
        let row = Entity::find_by_id("dlg-call-id")
            .one(&db)
            .await
            .unwrap()
            .expect("alias row present");
        assert_eq!(row.direction, "alias", "direction must stay enum-width");
        assert_eq!(row.target_session_id.as_deref(), Some(target.as_str()));

        // Registry lookup + the cluster resolution path consumers rely on.
        let full = r.lookup("dlg-call-id").await.unwrap();
        assert!(full.is_alias());
        assert_eq!(full.canonical_session_id(), target);
        let (owner, canonical) = crate::call::runtime::resolve_owner_and_session(
            &(r.clone() as super::super::SessionRegistryRef),
            "dlg-call-id",
        )
        .await
        .unwrap();
        assert_eq!(owner, "node-1");
        assert_eq!(canonical, target);

        // Upsert must update the target (OnConflict column list includes
        // TargetSessionId) alongside node ownership.
        let new_target = "sess-reassigned".to_string();
        r.register(&SessionInfo::dialog_alias(
            "dlg-call-id",
            new_target.clone(),
            "node-2",
        ))
        .await
        .unwrap();
        let updated = r.lookup("dlg-call-id").await.unwrap();
        assert_eq!(updated.node_id, "node-2");
        assert_eq!(updated.canonical_session_id(), new_target);
        assert_eq!(updated.target_session_id.as_deref(), Some(new_target.as_str()));
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
    async fn heartbeat_node_refreshes_only_live_rows() {
        let (r, db) = reg_short().await;
        r.register(&info("own1", "node-1")).await.unwrap();
        r.register(&info("own2", "node-1")).await.unwrap();
        r.register(&info("other", "node-2")).await.unwrap();

        // Backdate both node-1 rows so the heartbeat "due" filter passes.
        backdate_row(&db, "own1", 120, 120).await;
        backdate_row(&db, "own2", 120, 120).await;

        // Only "own1" is reported live: "own2" (the ghost) must stay stale.
        r.heartbeat_node("node-1", &["own1".to_string()])
            .await
            .unwrap();

        let own1_fresh = row_freshness(&db, "own1").await.unwrap();
        let own2_fresh = row_freshness(&db, "own2").await.unwrap();
        assert!(
            own1_fresh > chrono::Utc::now() - chrono::Duration::seconds(30),
            "live row must be refreshed, got {own1_fresh}"
        );
        assert!(
            own2_fresh < chrono::Utc::now() - chrono::Duration::seconds(60),
            "ghost row must be left untouched, got {own2_fresh}"
        );

        // The sweeper reclaims the stale ghost; everything live/fresh stays.
        r.sweep().await.unwrap();
        assert!(r.lookup("own1").await.is_some());
        assert!(
            r.lookup("own2").await.is_none(),
            "ghost row swept after TTL"
        );
        assert!(r.lookup("other").await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_node_with_empty_live_list_is_noop() {
        let (r, db) = reg_short().await;
        r.register(&info("own1", "node-1")).await.unwrap();
        backdate_row(&db, "own1", 120, 120).await;

        r.heartbeat_node("node-1", &[]).await.unwrap();
        let fresh = row_freshness(&db, "own1").await.unwrap();
        assert!(
            fresh < chrono::Utc::now() - chrono::Duration::seconds(60),
            "no live ids → nothing refreshed, got {fresh}"
        );
    }

    /// Defense-in-depth: a row can be heartbeat-fresh yet ancient by
    /// `started_at` (the immortal-ghost scenario).  The age cut must delete
    /// it regardless of freshness.
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_deletes_rows_older_than_max_age_even_when_fresh() {
        let (r, _db) = reg_short().await;

        let mut ancient = info("ancient", "node-1");
        ancient.started_at = chrono::Utc::now() - chrono::Duration::seconds(400);
        r.register(&ancient).await.unwrap();
        r.register(&info("young", "node-1")).await.unwrap();

        // register leaves last_updated_at = now (heartbeat-fresh) while
        // started_at is ancient — exactly the immortal-ghost shape.
        r.sweep().await.unwrap();

        assert!(
            r.lookup("ancient").await.is_none(),
            "age-bound ghost must be swept despite fresh last_updated_at"
        );
        assert!(r.lookup("young").await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_check_ok() {
        let r = reg().await;
        assert!(r.health_check().await.is_ok());
    }
}
