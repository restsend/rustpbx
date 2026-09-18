use sea_orm_migration::prelude::*;

use super::cluster_session;

/// Add `target_session_id` to `cluster_sessions`.
///
/// Dialog Call-ID alias rows used to stash `"alias:<session_id>"` inside the
/// short `direction` enum column (VARCHAR(16)). On strict deployments (MySQL,
/// PostgreSQL) every alias insert failed with `Data too long for column
/// 'direction'`, silently breaking cross-node dialog Call-ID resolution.
/// Alias rows now keep `direction = 'alias'` and carry the canonical session
/// id in this dedicated column (width aligned with `call_id`: session ids can
/// be verbatim SIP Call-IDs).
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager
            .has_column("cluster_sessions", "target_session_id")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(cluster_session::Entity)
                        .add_column(
                            ColumnDef::new(cluster_session::Column::TargetSessionId)
                                .string_len(200)
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager
            .has_column("cluster_sessions", "target_session_id")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(cluster_session::Entity)
                        .drop_column(cluster_session::Column::TargetSessionId)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
    use sea_orm_migration::MigratorTrait;

    async fn temp_db() -> DatabaseConnection {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustpbx-cluster-session-mig-{}-{nanos}-{seq}.sqlite3",
            std::process::id()
        ));
        let url = format!("sqlite://{}", path.display());
        crate::prepare_sqlite_database(&url)
            .await
            .expect("prepare sqlite file");
        sea_orm::Database::connect(&url).await.expect("connect")
    }

    async fn sqlite_row(db: &DatabaseConnection, sql: &str) -> Option<(i64, String)> {
        let rows = db
            .query_all_raw(Statement::from_string(db.get_database_backend(), sql))
            .await
            .expect("query");
        rows.first().and_then(|r| {
            let count: i64 = r.try_get("", "v").ok()?;
            let col_type: String = r.try_get("", "type").ok()?;
            Some((count, col_type))
        })
    }

    async fn sqlite_scalar(db: &DatabaseConnection, sql: &str) -> i64 {
        let rows = db
            .query_all_raw(Statement::from_string(db.get_database_backend(), sql))
            .await
            .expect("query");
        rows.first()
            .and_then(|r| r.try_get::<i64>("", "v").ok())
            .unwrap_or(0)
    }

    /// Existence + declared width of both columns after a full migration.
    /// Guards against the two failure modes that broke production
    /// (incident 2026-09-17): the column missing entirely, and the alias
    /// payload regressing into the 16-wide `direction` enum column.
    #[tokio::test]
    async fn target_session_id_column_and_widths_after_full_migration() {
        let db = temp_db().await;
        crate::migration::Migrator::up(&db, None).await.expect("up");

        let (count, col_type) = sqlite_row(
            &db,
            "SELECT COUNT(*) AS v, type AS type FROM pragma_table_info('cluster_sessions') WHERE name = 'target_session_id'",
        )
        .await
        .expect("pragma row");
        assert_eq!(count, 1, "target_session_id column must exist");
        assert_eq!(col_type, "varchar(200)", "target_session_id must stay 200 wide");

        let (dir_count, dir_type) = sqlite_row(
            &db,
            "SELECT COUNT(*) AS v, type AS type FROM pragma_table_info('cluster_sessions') WHERE name = 'direction'",
        )
        .await
        .expect("pragma row");
        assert_eq!(dir_count, 1);
        assert_eq!(dir_type, "varchar(16)", "direction must stay the short enum column");
    }

    /// Simulate a legacy deployment whose `cluster_sessions` predates this
    /// migration, then verify the incremental migration upgrades it in place
    /// (the guarded ADD COLUMN path) without touching existing rows.
    #[tokio::test]
    async fn incremental_migration_upgrades_legacy_schema() {
        let db = temp_db().await;
        // Legacy pre-migration table shape (no target_session_id).
        sea_orm::ConnectionTrait::execute_unprepared(
            &db,
            "CREATE TABLE `cluster_sessions` (\
                `call_id` varchar(200) NOT NULL PRIMARY KEY,\
                `node_id` varchar(64) NOT NULL,\
                `caller` varchar(160) NOT NULL,\
                `callee` varchar(160) NOT NULL,\
                `direction` varchar(16) NOT NULL,\
                `started_at` timestamp DEFAULT CURRENT_TIMESTAMP,\
                `last_updated_at` timestamp DEFAULT CURRENT_TIMESTAMP\
            )",
        )
        .await
        .expect("create legacy table");
        sea_orm::ConnectionTrait::execute_unprepared(
            &db,
            "INSERT INTO `cluster_sessions` (`call_id`, `node_id`, `caller`, `callee`, `direction`) \
             VALUES ('legacy-call', 'node-1', '1001', '1002', 'inbound')",
        )
        .await
        .expect("seed legacy row");

        crate::migration::Migrator::up(&db, None).await.expect("up");

        let (count, col_type) = sqlite_row(
            &db,
            "SELECT COUNT(*) AS v, type AS type FROM pragma_table_info('cluster_sessions') WHERE name = 'target_session_id'",
        )
        .await
        .expect("pragma row");
        assert_eq!(count, 1, "incremental migration must add target_session_id");
        assert_eq!(col_type, "varchar(200)");

        // The pre-existing row survives the upgrade.
        let legacy_rows = sqlite_scalar(
            &db,
            "SELECT COUNT(*) AS v FROM cluster_sessions WHERE call_id = 'legacy-call'",
        )
        .await;
        assert_eq!(legacy_rows, 1, "legacy row must survive the upgrade");
    }

    /// Alias-row write path against the real migrated schema, plus down().
    #[tokio::test]
    async fn alias_row_roundtrip_and_down() {
        use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
        let db = temp_db().await;
        crate::migration::Migrator::up(&db, None).await.expect("up");

        // A session id at the full column width (verbatim Call-IDs reach this
        // size) must persist — this is the payload that used to overflow
        // `direction`.
        let long_target = "s".repeat(200);
        let active = super::cluster_session::ActiveModel {
            call_id: Set("dlg-call-id".to_string()),
            node_id: Set("node-1".to_string()),
            caller: Set(String::new()),
            callee: Set(String::new()),
            direction: Set("alias".to_string()),
            target_session_id: Set(Some(long_target.clone())),
            ..Default::default()
        };
        super::cluster_session::Entity::insert(active)
            .exec(&db)
            .await
            .expect("insert alias row");

        let row = super::cluster_session::Entity::find()
            .filter(super::cluster_session::Column::CallId.eq("dlg-call-id"))
            .one(&db)
            .await
            .expect("query")
            .expect("alias row present");
        assert_eq!(row.direction, "alias");
        assert_eq!(row.target_session_id.as_deref(), Some(long_target.as_str()));

        // down() drops the column again. Targeted (not Migrator::down) so the
        // test stays independent of this migration's position in the list.
        use sea_orm_migration::MigrationTrait;
        super::Migration.down(&sea_orm_migration::SchemaManager::new(&db))
            .await
            .expect("down");
        let dropped = sqlite_scalar(
            &db,
            "SELECT COUNT(*) AS v FROM pragma_table_info('cluster_sessions') WHERE name = 'target_session_id'",
        )
        .await;
        assert_eq!(dropped, 0, "down() must drop target_session_id");
    }
}
