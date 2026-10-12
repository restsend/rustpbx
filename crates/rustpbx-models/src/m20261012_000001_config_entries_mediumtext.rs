//! Widen `config_entries.content` from TEXT to MEDIUMTEXT.
//!
//! Field audit (2026-10-12): `config_entries` is the generic config store
//! (`src/config_store.rs`) persisting generated TOML documents (IVR flows,
//! queue files, ...) as single `content` rows. The original creation used
//! 64 KB TEXT; a large generated IVR/dialplan document can exceed that and
//! fail the write in strict MySQL (1406 Data too long). MEDIUMTEXT (16 MB)
//! mirrors the `cc_entity_extras.value` widening from m20260929_000001.
//!
//! Backend notes: SQLite's TEXT is unlimited — nothing to change. Postgres
//! `text` is already unlimited (there is no `mediumtext`), so the statement
//! is a metadata-only no-op kept for symmetry with the other widening
//! migrations. `content` is not part of any index (the only index spans
//! `category, entry_name`), so no index rebuild is needed.

use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// MySQL helper: current `DATA_TYPE` of a column, or `None` when the column
/// does not exist.
async fn mysql_column_data_type(
    db: &SchemaManagerConnection<'_>,
    table: &str,
    column: &str,
) -> Result<Option<String>, DbErr> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_string(
            DbBackend::MySql,
            format!(
                "SELECT DATA_TYPE FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() \
                   AND TABLE_NAME = '{table}' \
                   AND COLUMN_NAME = '{column}'"
            ),
        ))
        .await?;
    match row {
        Some(row) => {
            let data_type: String = row.try_get("", "DATA_TYPE")?;
            Ok(Some(data_type))
        }
        None => Ok(None),
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match db.get_database_backend() {
            DbBackend::MySql => {
                // Idempotent: a DBA may apply the equivalent SQL by hand
                // before the upgrade; re-running this migration must be a
                // no-op then (information_schema-guarded ALTER below).
                if mysql_column_data_type(db, "config_entries", "content")
                    .await?
                    .as_deref()
                    != Some("mediumtext")
                {
                    db.execute_unprepared(
                        "ALTER TABLE config_entries MODIFY COLUMN content MEDIUMTEXT NOT NULL",
                    )
                    .await?;
                }
            }
            DbBackend::Postgres => {
                // `text` is already unlimited; metadata-only statement.
                db.execute_unprepared(
                    "ALTER TABLE config_entries ALTER COLUMN content TYPE TEXT",
                )
                .await?;
            }
            // SQLite's TEXT is unlimited — existing tables need no rebuild.
            DbBackend::Sqlite => {}
            _ => {}
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match db.get_database_backend() {
            DbBackend::MySql => {
                // Best-effort only: values longer than 64 KB are truncated
                // on the way back; real deployments never run down().
                db.execute_unprepared(
                    "ALTER TABLE config_entries MODIFY COLUMN content TEXT NOT NULL",
                )
                .await?;
            }
            DbBackend::Postgres => {
                // `text` and `text`: no-op, kept for symmetry.
                db.execute_unprepared(
                    "ALTER TABLE config_entries ALTER COLUMN content TYPE TEXT",
                )
                .await?;
            }
            DbBackend::Sqlite => {}
            _ => {}
        }
        Ok(())
    }
}
