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
//! - SWEA sweeper deletes stale rows once per minute in bounded batches: ids
//!   are selected with a non-locking read, then deleted in short transactions
//!   with deadlock retry. MySQL cleanup uses the existing primary key at READ
//!   COMMITTED, avoiding range/gap locks against concurrent writers. The age
//!   cut advances through the primary key without repeatedly sorting the table.
//!   The age-based cut `started_at < now - max_age`
//!   bounds any possible ghost regardless of heartbeat freshness.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
#[cfg(test)]
use sea_orm::PaginatorTrait;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, ExprTrait,
    IdenStatic, IsolationLevel, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
    TransactionTrait,
};
use tokio::task::JoinHandle;

use super::{RegistryError, SessionInfo, SessionRegistry, SessionRegistryRef};
use crate::models::cluster_session::{ActiveModel as SessionActiveModel, Column, Entity, Model};
use crate::utils;

/// SWEA sweeper cadence.
const SWEEPER_INTERVAL: Duration = Duration::from_secs(60);

/// Rows deleted per sweep transaction. Bounded chunks keep each DELETE a
/// short record-lock transaction instead of an index-range scan.
const SWEEP_BATCH_SIZE: usize = 500;

/// Upper bound on batches per column per sweep tick, so a pathological
/// backlog cannot pin the sweeper task forever.
const SWEEP_MAX_BATCHES: usize = 200;

/// Retries for a chunk that lost an InnoDB deadlock race (MySQL 1213 /
/// SQLSTATE 40001, Postgres 40P01). The victim is rolled back instantly, so
/// retrying the same chunk is always safe.
const SWEEP_DEADLOCK_RETRIES: u32 = 3;

#[cfg(test)]
tokio::task_local! {
    static BEFORE_SWEEP_DELETE: std::cell::RefCell<Option<futures::future::BoxFuture<'static, ()>>>;
}

/// Classify a deadlocked statement: the transaction was chosen as victim and
/// rolled back immediately — the statement can simply be retried. Matches
/// MySQL ("1213 (40001): Deadlock found when trying to get lock") and
/// Postgres (SQLSTATE 40P01, "deadlock detected").
fn is_deadlock(error: &sea_orm::DbErr) -> bool {
    let text = error.to_string();
    text.contains("40001") || text.to_ascii_lowercase().contains("deadlock")
}

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
        self.sweep_stale(
            Column::LastUpdatedAt,
            freshness_cutoff,
            SWEEP_BATCH_SIZE,
            SWEEP_MAX_BATCHES,
        )
        .await?;

        let age_cutoff =
            now - chrono::Duration::from_std(self.max_age).unwrap_or(chrono::Duration::hours(12));
        self.sweep_stale(
            Column::StartedAt,
            age_cutoff,
            SWEEP_BATCH_SIZE,
            SWEEP_MAX_BATCHES,
        )
        .await
    }

    /// Delete rows whose `column` is older than `cutoff`, in bounded batches.
    ///
    /// The unindexed age cut walks the existing primary key once per tick.
    /// The freshness cut retains its indexed timestamp lookup. Selection is
    /// non-locking; deletion rechecks staleness so a new heartbeat wins safely.
    async fn sweep_stale(
        &self,
        column: Column,
        cutoff: chrono::DateTime<chrono::Utc>,
        batch_size: usize,
        max_batches: usize,
    ) -> Result<(), sea_orm::DbErr> {
        let by_age = matches!(column, Column::StartedAt);
        let mut after: Option<String> = None;
        for _ in 0..max_batches {
            let mut query = Entity::find()
                .select_only()
                .column(Column::CallId)
                .filter(column.lt(cutoff));
            if by_age {
                if let Some(after) = &after {
                    query = query.filter(Column::CallId.gt(after.clone()));
                }
                query = query.order_by_asc(Column::CallId);
            } else {
                query = query.order_by_asc(column);
            }
            let mut ids: Vec<String> = query
                .limit(batch_size as u64)
                .into_tuple::<String>()
                .all(&self.db)
                .await?;
            if ids.is_empty() {
                return Ok(());
            }
            if by_age {
                after = ids.last().cloned();
            }
            // Keep write acquisition ordered independently of timestamp order.
            ids.sort_unstable();
            #[cfg(test)]
            if let Ok(Some(before_delete)) =
                BEFORE_SWEEP_DELETE.try_with(|hook| hook.borrow_mut().take())
            {
                before_delete.await;
            }
            let mut attempt = 0u32;
            loop {
                let result = self.delete_stale_batch(column, cutoff, &ids).await;
                match result {
                    Ok(_) => break,
                    Err(error) if attempt < SWEEP_DEADLOCK_RETRIES && is_deadlock(&error) => {
                        attempt += 1;
                        // Tiny deterministic jitter so concurrent nodes retry
                        // at different moments instead of lock-stepping.
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos())
                            .unwrap_or(0);
                        let delay =
                            25u64 * (1 << std::cmp::Ord::min(attempt, 4)) + u64::from(nanos % 20);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            if ids.len() < batch_size {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn delete_stale_batch(
        &self,
        column: Column,
        cutoff: chrono::DateTime<chrono::Utc>,
        ids: &[String],
    ) -> Result<(), sea_orm::DbErr> {
        if self.db.get_database_backend() != DbBackend::MySql {
            Entity::delete_many()
                .filter(Column::CallId.is_in(ids.iter().cloned()))
                .filter(column.lt(cutoff))
                .exec(&self.db)
                .await?;
            return Ok(());
        }

        // IN alone may still choose the timestamp index and lock fresh rows.
        // PRIMARY already exists on deployed schemas. READ COMMITTED also
        // avoids gap locks for IDs another sweeper deleted after selection.
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut values: Vec<sea_orm::Value> = ids.iter().cloned().map(Into::into).collect();
        values.push(cutoff.into());
        let statement = Statement::from_sql_and_values(
            DbBackend::MySql,
            format!(
                "DELETE `cluster_sessions` FROM `cluster_sessions` FORCE INDEX (`PRIMARY`) \
                 WHERE `call_id` IN ({placeholders}) AND `{}` < ?",
                column.as_str()
            ),
            values,
        );
        // Only this cleanup transaction changes isolation; pool/session defaults
        // and all business writers keep their existing behavior.
        let transaction = self
            .db
            .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
            .await?;
        transaction.execute_raw(statement).await?;
        transaction.commit().await
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
        let mut conflict = OnConflict::column(Column::CallId);
        for (column, value) in [
            (Column::NodeId, Expr::value(info.node_id.clone())),
            (Column::Caller, Expr::value(info.caller.clone())),
            (Column::Callee, Expr::value(info.callee.clone())),
            (Column::Direction, Expr::value(info.direction.clone())),
            (
                Column::TargetSessionId,
                Expr::value(info.target_session_id.clone()),
            ),
            (Column::LastUpdatedAt, Expr::value(chrono::Utc::now())),
        ] {
            // MySQL ignores an upsert WHERE clause; CASE keeps the priority atomic
            // on every backend when a home-proxy dialog reuses a session's id.
            let value = if info.is_alias() {
                Expr::case(
                    Expr::col((Entity, Column::Direction))
                        .eq(SessionInfo::ALIAS_DIRECTION)
                        .or(Expr::col((Entity, Column::Direction)).like("alias:%")),
                    value,
                )
                .finally(Expr::col((Entity, column)))
                .into()
            } else {
                value
            };
            conflict.value(column, value);
        }
        Entity::insert(model)
            .on_conflict(conflict.to_owned())
            // Registering an unchanged session is successful even when MySQL changes no rows.
            .exec_without_returning(&self.db)
            .await
            .map_err(|e| RegistryError::Unavailable(e.to_string()))?;
        Ok(())
    }

    async fn unregister(&self, info: &SessionInfo) -> Result<(), RegistryError> {
        let query = Entity::delete_many()
            .filter(Column::CallId.eq(&info.call_id))
            .filter(Column::NodeId.eq(&info.node_id))
            .filter(Column::Direction.eq(&info.direction));
        let query = match &info.target_session_id {
            Some(target) => query.filter(Column::TargetSessionId.eq(target)),
            None => query.filter(Column::TargetSessionId.is_null()),
        };
        query.exec(&self.db)
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
        match self.lookup_owner_checked(call_id).await {
            Ok(row) => row,
            Err(_) => None, // already logged by `lookup_owner_checked`
        }
    }

    /// DB-backed override: a query failure surfaces as
    /// [`RegistryError::Unavailable`] instead of collapsing into "not found",
    /// so cluster callers can answer 503 rather than a misleading 404.
    async fn lookup_owner_checked(&self, call_id: &str) -> Result<Option<String>, RegistryError> {
        match Entity::find()
            .select_only()
            .column(Column::NodeId)
            .filter(Column::CallId.eq(call_id))
            .into_tuple::<(String,)>()
            .one(&self.db)
            .await
        {
            Ok(row) => Ok(row.map(|(node_id,)| node_id)),
            // A DB outage must be distinguishable from "not found" — surface
            // it loudly instead of silently reporting no owner.
            Err(e) => {
                tracing::error!(call_id, error = %e, "session registry lookup_owner failed");
                Err(RegistryError::Unavailable(e.to_string()))
            }
        }
    }

    async fn lookup(&self, call_id: &str) -> Option<SessionInfo> {
        match Entity::find_by_id(call_id).one(&self.db).await {
            Ok(row) => row.map(from_model),
            Err(e) => {
                tracing::error!(call_id, error = %e, "session registry lookup failed");
                None
            }
        }
    }
}

#[cfg(test)]
impl DbSessionRegistry {
    pub(crate) async fn list_all(&self, limit: usize) -> Vec<SessionInfo> {
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

    pub(crate) async fn list_by_node(&self, node_id: &str) -> Vec<String> {
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

    pub(crate) async fn active_count(&self) -> usize {
        Entity::find().count(&self.db).await.unwrap_or(0) as usize
    }

    pub(crate) async fn health_check(&self) -> Result<(), RegistryError> {
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

    #[tokio::test]
    async fn register_accepts_mysql_noop_and_propagates_sql_errors() {
        use sea_orm::{DbBackend, DbErr, MockDatabase, MockExecResult};

        // SQLite RETURNING cannot reproduce MySQL's successful zero-row upsert.
        for (rows, sql_error) in [(0, false), (1, false), (2, false), (0, true)] {
            let sql = MockDatabase::new(DbBackend::MySql);
            let db = if sql_error {
                sql.append_exec_errors([DbErr::Custom("registry storage unavailable".into())])
            } else {
                sql.append_exec_results([MockExecResult {
                    last_insert_id: 0,
                    rows_affected: rows,
                }])
            }
            .into_connection();
            let registry = DbSessionRegistry::new(
                db,
                Duration::from_secs(3600),
                Duration::from_secs(crate::call::runtime::DEFAULT_SESSION_MAX_AGE_SECS),
            );
            registry.sweeper_cancel.cancel();
            let result = registry.register(&info("call-test", "node-test")).await;
            assert_eq!(result.is_err(), sql_error, "rows={rows}: {result:?}");
            if sql_error {
                assert!(matches!(result, Err(RegistryError::Unavailable(message))
                    if message.contains("registry storage unavailable")));
            }
        }
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
        r.unregister(&info("a", "node-1")).await.unwrap();
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

    /// The chunked sweeper must drain more stale rows than one batch holds —
    /// exercising the select-ids → delete-by-pk loop with a batch size
    /// smaller than the backlog — while leaving fresh rows untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_stale_drains_more_rows_than_one_batch() {
        let (r, db) = reg_short().await;

        for i in 0..5 {
            let call_id = format!("stale-{i}");
            r.register(&info(&call_id, "node-old")).await.unwrap();
            // TTL is 60s in this fixture → 120s ago is stale by freshness.
            backdate_row(&db, &call_id, 120, 120).await;
        }
        r.register(&info("fresh", "node-1")).await.unwrap();

        // Batch of 2 against a backlog of 5 → three select/delete rounds.
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(60);
        r.sweep_stale(Column::LastUpdatedAt, cutoff, 2, 50)
            .await
            .unwrap();

        for i in 0..5 {
            assert!(
                r.lookup(&format!("stale-{i}")).await.is_none(),
                "stale-{i} must be swept across batch boundaries"
            );
        }
        assert!(r.lookup("fresh").await.is_some());
    }

    #[tokio::test]
    async fn sweep_preserves_heartbeat_and_handles_peer_deleted_candidates() {
        use futures::FutureExt;
        let (registry, db) = reg_short().await;
        for id in ["refreshed", "peer-deleted", "expired"] {
            registry.register(&info(id, "node-1")).await.unwrap();
            backdate_row(&db, id, 120, 120).await;
        }
        let peer = registry.clone();
        let between_read_and_delete = async move {
            peer.heartbeat_node("node-1", &["refreshed".into()])
                .await
                .unwrap();
            peer.unregister(&info("peer-deleted", "node-1"))
                .await
                .unwrap();
        }
        .boxed();
        BEFORE_SWEEP_DELETE
            .scope(
                std::cell::RefCell::new(Some(between_read_and_delete)),
                registry.sweep(),
            )
            .await
            .unwrap();
        assert!(registry.lookup("refreshed").await.is_some());
        assert!(registry.lookup("peer-deleted").await.is_none());
        assert!(registry.lookup("expired").await.is_none());
    }

    // These fixtures create only uniquely named test databases on the supplied
    // server. The configured URL is a server URL without a business database.
    async fn mysql_sweep_fixture() -> (DatabaseConnection, DatabaseConnection, String) {
        use sea_orm::ConnectionTrait;
        let base = std::env::var("RUSTPBX_TEST_MYSQL_URL")
            .expect("RUSTPBX_TEST_MYSQL_URL must point at an isolated MySQL server");
        let name = format!("rustpbx_sweep_test_{}", uuid::Uuid::new_v4().simple());
        let admin = sea_orm::Database::connect(&base).await.unwrap();
        admin
            .execute_unprepared(&format!("CREATE DATABASE `{name}`"))
            .await
            .unwrap();
        let mut options =
            sea_orm::ConnectOptions::new(format!("{}/{name}", base.trim_end_matches('/')));
        // A single pool connection makes session handler counters meaningful.
        options.max_connections(1).min_connections(1);
        let db = sea_orm::Database::connect(options).await.unwrap();
        crate::models::cluster_session::Migration
            .up(&SchemaManager::new(&db))
            .await
            .unwrap();
        db.execute_unprepared("SET SESSION innodb_lock_wait_timeout = 1")
            .await
            .unwrap();
        db.execute_unprepared("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .await
            .unwrap();
        (admin, db, name)
    }

    async fn seed_mysql_sweep_rows(
        db: &DatabaseConnection,
        fresh: usize,
        expired: usize,
        by_age: bool,
    ) {
        use chrono::Timelike;
        use sea_orm::ConnectionTrait;
        let now = chrono::Utc::now().with_nanosecond(0).unwrap();
        let old = now - chrono::Duration::hours(13);
        for start in (0..fresh + expired).step_by(500) {
            Entity::insert_many((start..(start + 500).min(fresh + expired)).map(|i| {
                let stale = i >= fresh;
                SessionActiveModel {
                    call_id: Set(format!("{}-{i:05}", if stale { "stale" } else { "fresh" })),
                    node_id: Set("node-1".into()),
                    caller: Set("1001".into()),
                    callee: Set("1002".into()),
                    direction: Set("inbound".into()),
                    target_session_id: Set(None),
                    started_at: Set(if stale && by_age { old } else { now }),
                    last_updated_at: Set(if stale && !by_age { old } else { now }),
                }
            }))
            .exec_without_returning(db)
            .await
            .unwrap();
        }
        db.execute_unprepared("ANALYZE TABLE cluster_sessions")
            .await
            .unwrap();
    }

    async fn mysql_handler_reads(db: &DatabaseConnection) -> u64 {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        db.query_all_raw(Statement::from_string(
            DbBackend::MySql,
            "SHOW SESSION STATUS WHERE Variable_name IN \
             ('Handler_read_rnd_next','Handler_read_next','Handler_read_first','Handler_read_key')",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            row.try_get::<String>("", "Value")
                .unwrap()
                .parse::<u64>()
                .unwrap()
        })
        .sum()
    }

    #[tokio::test]
    #[ignore = "requires RUSTPBX_TEST_MYSQL_URL pointing at an isolated MySQL server"]
    async fn mysql_sweep_does_not_lock_unselected_fresh_rows() {
        use futures::FutureExt;
        use sea_orm::{ConnectionTrait, QuerySelect, TransactionTrait};
        let (admin, db, name) = mysql_sweep_fixture().await;
        seed_mysql_sweep_rows(&db, 10_000, 500, false).await;
        let registry = DbSessionRegistry::new(
            db.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(43200),
        );
        registry.sweeper_cancel.cancel();
        let base = std::env::var("RUSTPBX_TEST_MYSQL_URL").unwrap();
        let peer_db = sea_orm::Database::connect(format!("{}/{name}", base.trim_end_matches('/')))
            .await
            .unwrap();
        let holder = peer_db.begin().await.unwrap();
        Entity::find_by_id("fresh-00000")
            .lock_exclusive()
            .one(&holder)
            .await
            .unwrap()
            .unwrap();
        // A busy writer of a fresh record outside the candidate IDs must not
        // delay cleanup. The old range-index DELETE times out on this row.
        let peer = DbSessionRegistry::new(
            peer_db.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(43200),
        );
        peer.sweeper_cancel.cancel();
        let between_read_and_delete = async move {
            peer.unregister(&info("stale-10000", "node-1"))
                .await
                .unwrap();
            peer.heartbeat_node("node-1", &["stale-10001".into()])
                .await
                .unwrap();
        }
        .boxed();
        let result = BEFORE_SWEEP_DELETE
            .scope(
                std::cell::RefCell::new(Some(between_read_and_delete)),
                registry.sweep(),
            )
            .await;
        holder.rollback().await.unwrap();
        let fresh_survived = registry.lookup("fresh-00000").await.is_some();
        let heartbeat_survived = registry.lookup("stale-10001").await.is_some();
        let remaining = registry.active_count().await;
        let isolation: String = db
            .query_one_raw(Statement::from_string(
                DbBackend::MySql,
                "SELECT @@session.transaction_isolation AS isolation_level",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "isolation_level")
            .unwrap();
        drop(registry);
        db.close().await.unwrap();
        peer_db.close().await.unwrap();
        admin
            .execute_unprepared(&format!("DROP DATABASE `{name}`"))
            .await
            .unwrap();
        assert!(
            result.is_ok(),
            "cleanup locked an unselected fresh row: {result:?}"
        );
        assert!(fresh_survived);
        assert!(heartbeat_survived);
        assert_eq!(remaining, 10_001);
        assert_eq!(
            isolation, "REPEATABLE-READ",
            "cleanup must not change pool defaults"
        );
    }

    #[tokio::test]
    #[ignore = "requires RUSTPBX_TEST_MYSQL_URL pointing at an isolated MySQL server"]
    async fn mysql_age_sweep_reads_table_once_across_batches() {
        use sea_orm::ConnectionTrait;
        let (admin, db, name) = mysql_sweep_fixture().await;
        seed_mysql_sweep_rows(&db, 10_000, 1_000, true).await;
        let registry = DbSessionRegistry::new(
            db.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(43200),
        );
        registry.sweeper_cancel.cancel();
        let before = mysql_handler_reads(&db).await;
        registry.sweep().await.unwrap();
        let reads = mysql_handler_reads(&db).await - before;
        let remaining = registry.active_count().await;
        drop(registry);
        db.close().await.unwrap();
        admin
            .execute_unprepared(&format!("DROP DATABASE `{name}`"))
            .await
            .unwrap();
        eprintln!("MySQL age sweep: 11000 rows, 1000 expired, {reads} handler reads");
        assert_eq!(remaining, 10_000);
        assert!(
            reads < 24_000,
            "age cleanup repeatedly scanned the fresh prefix: {reads}"
        );
    }

    #[test]
    fn deadlock_classification_matches_victim_errors() {
        let mysql = sea_orm::DbErr::Custom(
            "error returned from database: 1213 (40001): Deadlock found when trying to get lock; try restarting transaction"
                .into(),
        );
        assert!(is_deadlock(&mysql));
        let postgres = sea_orm::DbErr::Custom(
            "error returned from database: 40P01: deadlock detected".into(),
        );
        assert!(is_deadlock(&postgres));
        let transient = sea_orm::DbErr::Custom("connection refused".into());
        assert!(!is_deadlock(&transient));
    }
}
