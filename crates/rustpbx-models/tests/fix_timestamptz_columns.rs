use chrono::{DateTime, Utc};
use rustpbx_models::{config_entry, fix_timestamptz_columns::Migration, migration::Migrator};
use sea_orm::{ConnectionTrait, Database, DbBackend, EntityTrait, Statement, TransactionTrait};
use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

const COLUMNS: &[(&str, &str)] = &[
    ("config_entries", "created_at"),
    ("config_entries", "updated_at"),
    ("cluster_sessions", "started_at"),
    ("cluster_sessions", "last_updated_at"),
    ("rustpbx_cdr_daily", "day"),
    ("rustpbx_cdr_daily", "updated_at"),
    ("rustpbx_runtime_snapshots", "created_at"),
];

async fn column_type<C: ConnectionTrait>(db: &C, table: &str, column: &str) -> String {
    db.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT data_type::text AS t FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
        [table.into(), column.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "t")
    .unwrap()
}

#[tokio::test]
#[ignore = "requires RUSTPBX_POSTGRES_TEST_URL pointing to a PostgreSQL test database"]
async fn postgres_timestamp_columns_become_timestamptz() {
    let db = Database::connect(std::env::var("RUSTPBX_POSTGRES_TEST_URL").unwrap())
        .await
        .unwrap();
    let tx = db.begin().await.unwrap();
    tx.execute_unprepared("CREATE SCHEMA fix_timestamptz_test")
        .await
        .unwrap();
    tx.execute_unprepared("SET LOCAL search_path TO fix_timestamptz_test")
        .await
        .unwrap();
    tx.execute_unprepared("SET LOCAL TIME ZONE 'Asia/Shanghai'")
        .await
        .unwrap();

    Migrator::up(&tx, None).await.unwrap();
    for (table, column) in COLUMNS {
        assert_eq!(
            column_type(&tx, table, column).await,
            "timestamp with time zone"
        );
        // Recreate the schema an older release left behind.
        tx.execute_unprepared(&format!(
            "ALTER TABLE {table} ALTER COLUMN {column} TYPE TIMESTAMP"
        ))
        .await
        .unwrap();
    }

    // What the app stored: sqlx connects with TimeZone=UTC, so naive columns
    // hold UTC wall-clock values. The Shanghai session above makes sure the
    // conversion does not depend on the migrating session's time zone.
    tx.execute_unprepared(
        "INSERT INTO config_entries (category, entry_name, content, created_at, updated_at) \
         VALUES ('routing', 'a', 'x', '2026-10-19 05:55:49', '2026-10-19 05:55:49')",
    )
    .await
    .unwrap();
    assert!(config_entry::Entity::find().all(&tx).await.is_err());

    // A column someone already fixed by hand must be left alone.
    tx.execute_unprepared("ALTER TABLE config_entries ALTER COLUMN updated_at TYPE TIMESTAMPTZ \
         USING updated_at AT TIME ZONE 'UTC'")
        .await
        .unwrap();

    let manager = SchemaManager::new(&tx);
    let expected: DateTime<Utc> = "2026-10-19T05:55:49Z".parse().unwrap();
    for _ in 0..2 {
        Migration.up(&manager).await.unwrap();
        for (table, column) in COLUMNS {
            assert_eq!(
                column_type(&tx, table, column).await,
                "timestamp with time zone"
            );
        }
        // NOT NULL and the CURRENT_TIMESTAMP default survive the conversion.
        let attrs = tx
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT is_nullable::text AS n, column_default::text AS d \
                 FROM information_schema.columns WHERE table_schema = current_schema() \
                 AND table_name = 'config_entries' AND column_name = 'created_at'",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attrs.try_get::<String>("", "n").unwrap(), "NO");
        assert_eq!(attrs.try_get::<String>("", "d").unwrap(), "CURRENT_TIMESTAMP");
        let row = config_entry::Entity::find()
            .one(&tx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((row.created_at, row.updated_at), (expected, expected));
    }
    tx.rollback().await.unwrap();
}
