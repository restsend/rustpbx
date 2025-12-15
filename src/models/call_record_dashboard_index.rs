use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;
        let col_started_at = crate::models::call_record::Column::StartedAt;
        let col_status = crate::models::call_record::Column::Status;
        let col_duration_secs = crate::models::call_record::Column::DurationSecs;

        // Composite index on started_at + status + duration_secs
        // This covers the dashboard query which filters by started_at and aggregates based on status and duration_secs
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_rustpbx_call_records_dashboard_stats")
                    .table(table)
                    .col(col_started_at)
                    .col(col_status)
                    .col(col_duration_secs)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_dashboard_stats")
                    .table(table)
                    .to_owned(),
            )
            .await
    }
}
