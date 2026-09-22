use rustpbx_models::{cdr_daily, runtime_snapshot};
use sea_orm::{ConnectionTrait, Database};
use sea_orm_migration::prelude::*;

#[tokio::test]
async fn report_migrations_resume_without_losing_rows() {
    // For MySQL verification, point this at a dedicated empty test database.
    let url = std::env::var("RUSTPBX_MIGRATION_TEST_URL")
        .unwrap_or_else(|_| "sqlite::memory:".into());
    let db = Database::connect(url).await.unwrap();
    let manager = SchemaManager::new(&db);
    let migrations: Vec<(Box<dyn MigrationTrait>, &str, &str)> = vec![
        (Box::new(cdr_daily::Migration), "rustpbx_cdr_daily", "idx_cdr_daily_day_dims"),
        (Box::new(runtime_snapshot::Migration), "rustpbx_runtime_snapshots", "idx_runtime_snapshots_kind_time"),
    ];
    for (migration, table, index) in migrations {
        migration.up(&manager).await.unwrap();
        let insert = if table == "rustpbx_cdr_daily" {
            "INSERT INTO rustpbx_cdr_daily (day, direction, updated_at) VALUES ('2026-09-22 00:00:00', 'inbound', '2026-09-22 00:00:00')"
        } else {
            "INSERT INTO rustpbx_runtime_snapshots (kind) VALUES ('locator')"
        };
        db.execute_unprepared(insert).await.unwrap();
        // Existing table and index, but no recorded migration completion.
        migration.up(&manager).await.unwrap();
        assert!(manager.has_index(table, index).await.unwrap());
        // Interrupted after table creation: recover the missing index as well.
        manager.drop_index(Index::drop().table(Alias::new(table)).name(index).to_owned()).await.unwrap();
        migration.up(&manager).await.unwrap();
        assert!(manager.has_index(table, index).await.unwrap());
        let row = db.query_one_raw(sea_orm::Statement::from_string(
            db.get_database_backend(), format!("SELECT COUNT(*) AS n FROM {table}"),
        )).await.unwrap().unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 1);
    }
}
