use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;
        let col_started_at = crate::models::call_record::Column::StartedAt;
        let col_from_number = crate::models::call_record::Column::FromNumber;
        let col_to_number = crate::models::call_record::Column::ToNumber;

        // Index on from_number
        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_from_number",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_from_number")
                        .table(table)
                        .col(col_from_number)
                        .to_owned(),
                )
                .await?;
        }

        // Composite index on started_at + from_number
        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_started_from",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_started_from")
                        .table(table)
                        .col(col_started_at)
                        .col(col_from_number)
                        .to_owned(),
                )
                .await?;
        }

        // Composite index on started_at + to_number
        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_started_to",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_started_to")
                        .table(table)
                        .col(col_started_at)
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
                    .name("idx_rustpbx_call_records_from_number")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_started_from")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_started_to")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}
