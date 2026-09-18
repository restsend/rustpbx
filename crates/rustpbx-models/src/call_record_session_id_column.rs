use sea_orm_migration::prelude::*;

/// Adds the logical-call correlation column to the CDR table.
///
/// `session_id` carries the root session id of the whole logical call
/// (RFC 7989 Session-ID inheritance): it equals `call_id` for the root
/// session and is shared by every child leg created by queue dispatch or
/// REFER transfer — the equivalent of FreeSWITCH's `linkedid` /
/// Asterisk's linked-id.  Child legs can be aggregated with
/// `GROUP BY session_id`, and the primary CDR of a logical call is
/// derived as `session_id IS NULL OR session_id = call_id`.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_call_records";

        if !manager.has_column(table_name, "session_id").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(super::call_record::Entity)
                        .add_column(
                            ColumnDef::new(super::call_record::Column::SessionId)
                                .string_len(120)
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index(table_name, "idx_rustpbx_call_records_session_id")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_session_id")
                        .table(super::call_record::Entity)
                        .col(super::call_record::Column::SessionId)
                        .if_not_exists()
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = super::call_record::Entity;

        if manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_session_id",
            )
            .await?
        {
            manager
                .drop_index(
                    Index::drop()
                        .name("idx_rustpbx_call_records_session_id")
                        .table(table)
                        .to_owned(),
                )
                .await?;
        }

        if manager
            .has_column("rustpbx_call_records", "session_id")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(table)
                        .drop_column(super::call_record::Column::SessionId)
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
            "rustpbx-session-id-mig-{}-{nanos}-{seq}.sqlite3",
            std::process::id()
        ));
        let url = format!("sqlite://{}", path.display());
        crate::prepare_sqlite_database(&url)
            .await
            .expect("prepare sqlite file");
        sea_orm::Database::connect(&url).await.expect("connect")
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

    #[tokio::test]
    async fn session_id_column_and_index_exist_after_full_migration() {
        let db = temp_db().await;
        crate::migration::Migrator::up(&db, None).await.expect("up");

        let cols = sqlite_scalar(
            &db,
            "SELECT COUNT(*) AS v FROM pragma_table_info('rustpbx_call_records') WHERE name = 'session_id'",
        )
        .await;
        assert_eq!(cols, 1, "session_id column must exist after migration");

        let indexes = sqlite_scalar(
            &db,
            "SELECT COUNT(*) AS v FROM sqlite_master WHERE type = 'index' AND name = 'idx_rustpbx_call_records_session_id'",
        )
        .await;
        assert_eq!(indexes, 1, "session_id index must exist after migration");
    }

    #[tokio::test]
    async fn session_id_roundtrip_and_down() {
        use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
        let db = temp_db().await;
        crate::migration::Migrator::up(&db, None).await.expect("up");

        // Rows sharing the logical-call root session id are insertable and
        // retrievable via the column.
        for (call_id, session_id) in [
            ("root-call", Some("root-call".to_string())),
            ("agent-leg", Some("root-call".to_string())),
            ("transfer-leg", Some("root-call".to_string())),
        ] {
            let active = super::super::call_record::ActiveModel {
                call_id: Set(call_id.to_string()),
                session_id: Set(session_id),
                direction: Set("inbound".to_string()),
                status: Set("completed".to_string()),
                started_at: Set(chrono::Utc::now()),
                duration_secs: Set(10),
                has_transcript: Set(false),
                transcript_status: Set("none".to_string()),
                created_at: Set(chrono::Utc::now()),
                updated_at: Set(chrono::Utc::now()),
                ..Default::default()
            };
            super::super::call_record::Entity::insert(active)
                .exec(&db)
                .await
                .expect("insert");
        }

        let legs = super::super::call_record::Entity::find()
            .filter(super::super::call_record::Column::SessionId.eq("root-call"))
            .all(&db)
            .await
            .expect("query by session_id");
        assert_eq!(legs.len(), 3, "all legs share the root session_id");

        // down() drops the index and the column without touching the rows.
        // Targeted (not Migrator::down) so the test stays independent of this
        // migration's position in the registry list.
        use sea_orm_migration::MigrationTrait;
        super::Migration.down(&sea_orm_migration::SchemaManager::new(&db))
            .await
            .expect("down");
        let cols = sqlite_scalar(
            &db,
            "SELECT COUNT(*) AS v FROM pragma_table_info('rustpbx_call_records') WHERE name = 'session_id'",
        )
        .await;
        assert_eq!(cols, 0, "session_id column must be dropped by down()");
        let indexes = sqlite_scalar(
            &db,
            "SELECT COUNT(*) AS v FROM sqlite_master WHERE type = 'index' AND name = 'idx_rustpbx_call_records_session_id'",
        )
        .await;
        assert_eq!(indexes, 0, "session_id index must be dropped by down()");
    }
}
