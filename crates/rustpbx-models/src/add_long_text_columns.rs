//! Widen columns that outgrow their VARCHAR bounds:
//! - `rustpbx_call_records.call_id` 120 → 255 (raw SIP Call-IDs overflow and
//!   drop CDRs in strict MySQL; the widened unique index stays in budget)
//! - `rustpbx_call_records.recording_url` → TEXT (signed object-store URLs)
//! - `presence_states.note` → TEXT (user free text)
//!
//! MySQL is guarded per-column via information_schema (a DBA-applied manual
//! fix replays as a no-op); Postgres statements are metadata-only; SQLite
//! needs nothing.

use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match db.get_database_backend() {
            DbBackend::MySql => {
                if mysql_call_id_needs_widening(db).await? {
                    db.execute_unprepared(
                        "ALTER TABLE rustpbx_call_records MODIFY COLUMN call_id VARCHAR(255) NOT NULL",
                    )
                    .await?;
                }
                if mysql_column_is_not_text(db, "rustpbx_call_records", "recording_url").await? {
                    db.execute_unprepared(
                        "ALTER TABLE rustpbx_call_records MODIFY COLUMN recording_url TEXT NULL",
                    )
                    .await?;
                }
                if mysql_column_is_not_text(db, "presence_states", "note").await? {
                    db.execute_unprepared(
                        "ALTER TABLE presence_states MODIFY COLUMN note TEXT NULL",
                    )
                    .await?;
                }
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "ALTER TABLE rustpbx_call_records ALTER COLUMN call_id TYPE VARCHAR(255)",
                )
                .await?;
                db.execute_unprepared(
                    "ALTER TABLE rustpbx_call_records ALTER COLUMN recording_url TYPE TEXT",
                )
                .await?;
                db.execute_unprepared(
                    "ALTER TABLE presence_states ALTER COLUMN note TYPE TEXT",
                )
                .await?;
            }
            // SQLite's VARCHAR(n) has TEXT affinity and never enforced the
            // length — existing tables need no rebuild.
            DbBackend::Sqlite => {}
            _ => {}
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match db.get_database_backend() {
            DbBackend::MySql => {
                // Best-effort: values longer than the original bounds are
                // truncated on the way back; real deployments never run down.
                db.execute_unprepared(
                    "ALTER TABLE rustpbx_call_records MODIFY COLUMN call_id VARCHAR(120) NOT NULL",
                )
                .await?;
                db.execute_unprepared(
                    "ALTER TABLE rustpbx_call_records MODIFY COLUMN recording_url VARCHAR(255) NULL",
                )
                .await?;
                db.execute_unprepared(
                    "ALTER TABLE presence_states MODIFY COLUMN note VARCHAR(255) NULL",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "ALTER TABLE rustpbx_call_records ALTER COLUMN call_id TYPE VARCHAR(120)",
                )
                .await?;
                db.execute_unprepared(
                    "ALTER TABLE rustpbx_call_records ALTER COLUMN recording_url TYPE VARCHAR(255)",
                )
                .await?;
                db.execute_unprepared(
                    "ALTER TABLE presence_states ALTER COLUMN note TYPE VARCHAR(255)",
                )
                .await?;
            }
            DbBackend::Sqlite => {}
            _ => {}
        }
        Ok(())
    }
}

/// MySQL helper: true while `rustpbx_call_records.call_id` is still missing
/// the VARCHAR(255) widening (either not varchar or shorter than 255).
async fn mysql_call_id_needs_widening(db: &SchemaManagerConnection<'_>) -> Result<bool, DbErr> {
    let Some(row) = mysql_column_info(db, "rustpbx_call_records", "call_id").await? else {
        // Column missing entirely → let the caller's ALTER fail loudly.
        return Ok(true);
    };
    let (data_type, max_len): (String, Option<i64>) = row;
    Ok(data_type != "varchar" || max_len.unwrap_or(0) < 255)
}

/// MySQL helper: true while the column is still a non-TEXT type (i.e. the
/// TEXT widening has not been applied yet — possibly by a DBA running the
/// manual SQL from `ddl/` ahead of the upgrade).
async fn mysql_column_is_not_text(
    db: &SchemaManagerConnection<'_>,
    table: &str,
    column: &str,
) -> Result<bool, DbErr> {
    let Some(row) = mysql_column_info(db, table, column).await? else {
        return Ok(true);
    };
    let (data_type, _): (String, Option<i64>) = row;
    Ok(data_type != "text")
}

/// Fetch `(DATA_TYPE, CHARACTER_MAXIMUM_LENGTH)` for a column, or `None`
/// when the column does not exist.
async fn mysql_column_info(
    db: &SchemaManagerConnection<'_>,
    table: &str,
    column: &str,
) -> Result<Option<(String, Option<i64>)>, DbErr> {
    let row = db
        .query_one_raw(sea_orm::Statement::from_string(
            DbBackend::MySql,
            format!(
                "SELECT DATA_TYPE, CHARACTER_MAXIMUM_LENGTH \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() \
                   AND TABLE_NAME = '{table}' \
                   AND COLUMN_NAME = '{column}'"
            ),
        ))
        .await?;
    match row {
        Some(row) => {
            let data_type: String = row.try_get("", "DATA_TYPE")?;
            let max_len: Option<i64> = row.try_get("", "CHARACTER_MAXIMUM_LENGTH")?;
            Ok(Some((data_type, max_len)))
        }
        None => Ok(None),
    }
}
