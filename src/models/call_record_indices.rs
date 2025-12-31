use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table = crate::models::call_record::Entity;
        let col_dept = crate::models::call_record::Column::DepartmentId;
        let col_trunk = crate::models::call_record::Column::SipTrunkId;
        let col_started_at = crate::models::call_record::Column::StartedAt;

        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_department",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_department")
                        .table(table)
                        .col(col_dept)
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index("rustpbx_call_records", "idx_rustpbx_call_records_sip_trunk")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_sip_trunk")
                        .table(table)
                        .col(col_trunk)
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index(
                "rustpbx_call_records",
                "idx_rustpbx_call_records_trunk_started",
            )
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .name("idx_rustpbx_call_records_trunk_started")
                        .table(table)
                        .col(col_trunk)
                        .col(col_started_at)
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
                    .name("idx_rustpbx_call_records_trunk_started")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_sip_trunk")
                    .table(table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_call_records_department")
                    .table(table)
                    .to_owned(),
            )
            .await
    }
}
