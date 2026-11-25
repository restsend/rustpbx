use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = super::call_record::Entity;
        let col_created_at = super::call_record::Column::CreatedAt;

        // Bare index on created_at. The other time index on this table is
        // (started_at, direction); queries that filter on created_at alone
        // (the per-agent status fallback scan and any retention/archive
        // sweeps) could not use it and degenerated into full table scans —
        // 12s+ wall time once the table holds more than a few thousand
        // wide (TEXT-column) rows.
        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_created_at",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_created_at")
                        .table(table)
                        .col(col_created_at)
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
                "idx_rustpbx_call_records_created_at",
            )
            .await?
        {
            manager
                .drop_index(
                    Index::drop()
                        .name("idx_rustpbx_call_records_created_at")
                        .table(table)
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

    const TABLE: &str = "rustpbx_call_records";
    const INDEX: &str = "idx_rustpbx_call_records_created_at";
    const MIGRATION: &str = "call_record_created_at_index";
    const PREVIOUS_MIGRATION: &str = "m20261012_000001_config_entries_mediumtext";

    struct LegacyMigrator;

    #[async_trait::async_trait]
    impl MigratorTrait for LegacyMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            Migrator::migrations()
                .into_iter()
                .filter(|migration| migration.name() != MIGRATION)
                .collect()
        }
    }

    #[tokio::test]
    async fn rolls_back_only_the_created_at_index_after_manual_creation() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let manager = SchemaManager::new(&db);
        LegacyMigrator::up(&db, None).await.unwrap();
        db.execute_unprepared(
            "CREATE INDEX idx_rustpbx_call_records_created_at ON rustpbx_call_records (created_at)",
        )
        .await
        .unwrap();
        Migrator::up(&db, Some(1)).await.unwrap();
        assert!(
            Migrator::get_pending_migrations(&db)
                .await
                .unwrap()
                .is_empty()
        );

        Migrator::down(&db, Some(1)).await.unwrap();
        let pending = Migrator::get_pending_migrations(&db).await.unwrap();
        let pending_names: Vec<_> = pending.iter().map(|migration| migration.name()).collect();
        assert_eq!(pending_names, [MIGRATION]);
        assert!(
            Migrator::get_migration_models(&db)
                .await
                .unwrap()
                .iter()
                .any(|migration| migration.version == PREVIOUS_MIGRATION)
        );
        assert!(!manager.has_index(TABLE, INDEX).await.unwrap());

        Migrator::up(&db, Some(1)).await.unwrap();
        assert!(manager.has_index(TABLE, INDEX).await.unwrap());
        assert!(
            Migrator::get_pending_migrations(&db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rolls_back_the_created_at_migration_when_the_index_is_already_absent() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("DROP INDEX idx_rustpbx_call_records_created_at")
            .await
            .unwrap();

        Migrator::down(&db, Some(1)).await.unwrap();
        let pending = Migrator::get_pending_migrations(&db).await.unwrap();
        let pending_names: Vec<_> = pending.iter().map(|migration| migration.name()).collect();
        assert_eq!(pending_names, [MIGRATION]);
    }
}
