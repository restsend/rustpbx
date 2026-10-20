use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

/// `DateTimeUtc` columns that older create migrations made as plain
/// `timestamp()`. On Postgres that is `TIMESTAMP WITHOUT TIME ZONE`, which sqlx
/// refuses to decode into `DateTime<Utc>`, so reads and `INSERT ... RETURNING`
/// fail. `rustpbx_call_records` is listed because the custom CDR creator could
/// have created it before the core migration did.
const COLUMNS: &[(&str, &[&str])] = &[
    ("config_entries", &["created_at", "updated_at"]),
    ("cluster_sessions", &["started_at", "last_updated_at"]),
    ("rustpbx_cdr_daily", &["day", "updated_at"]),
    ("rustpbx_runtime_snapshots", &["created_at"]),
    ("rustpbx_call_records", CALL_RECORD_COLUMNS),
];

pub const CALL_RECORD_COLUMNS: &[&str] = &[
    "started_at",
    "ended_at",
    "created_at",
    "updated_at",
    "archived_at",
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for (table, columns) in COLUMNS {
            fix_timestamptz_columns(db, table, columns).await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

/// Convert the listed columns of `table` to `TIMESTAMPTZ` on Postgres.
/// Columns that are already `TIMESTAMPTZ` (or missing) are skipped, and other
/// backends are left alone.
///
/// sqlx always connects with `TimeZone=UTC`, so the app's `DateTime<Utc>`
/// binds and `CURRENT_TIMESTAMP` defaults were stored as UTC wall-clock
/// values. Read them back as UTC explicitly rather than relying on the
/// migrating session's time zone. If another node converts the same column
/// between our check and ALTER, re-applying this under sqlx's UTC session
/// leaves the values unchanged.
pub async fn fix_timestamptz_columns<C: ConnectionTrait>(
    db: &C,
    table: &str,
    columns: &[&str],
) -> Result<(), DbErr> {
    if db.get_database_backend() != DbBackend::Postgres {
        return Ok(());
    }
    // Resolve the table through search_path like the ORM's unqualified
    // queries do. Only columns still on plain TIMESTAMP come back, so ones
    // already fixed (including by hand) are skipped.
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT attname::text AS column_name FROM pg_catalog.pg_attribute \
             WHERE attrelid = pg_catalog.to_regclass($1) \
             AND attnum > 0 AND NOT attisdropped \
             AND atttypid = 'timestamp without time zone'::pg_catalog.regtype",
            [format!("\"{table}\"").into()],
        ))
        .await?;
    let mut alterations = Vec::new();
    for row in rows {
        let column: String = row.try_get("", "column_name")?;
        if columns.contains(&column.as_str()) {
            alterations.push(format!(
                "ALTER COLUMN \"{column}\" TYPE TIMESTAMPTZ USING \"{column}\" AT TIME ZONE 'UTC'"
            ));
        }
    }
    if !alterations.is_empty() {
        db.execute_unprepared(&format!(
            "ALTER TABLE \"{table}\" {}",
            alterations.join(", ")
        ))
        .await?;
    }
    Ok(())
}
