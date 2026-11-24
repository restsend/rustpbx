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

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_created_at")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}
