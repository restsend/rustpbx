use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;
        let col_started_at = crate::models::call_record::Column::StartedAt;
        let col_id = crate::models::call_record::Column::Id;
        let col_status = crate::models::call_record::Column::Status;
        let col_to_number = crate::models::call_record::Column::ToNumber;

        // Composite index on started_at + id + status (for pagination and stats)
        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_started_id_status",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_started_id_status")
                        .table(table)
                        .col(col_started_at)
                        .col(col_id)
                        .col(col_status)
                        .to_owned(),
                )
                .await?;
        }

        // Index on to_number
        if !manager
            .has_index("rustpbx_call_records", "idx_rustpbx_call_records_to_number")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_to_number")
                        .table(table)
                        .col(col_to_number)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_to_number")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name("idx_rustpbx_call_records_tags")
                    .table(table)
                    .to_owned(),
            )
            .await
            .ok();

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_started_id_status")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}
