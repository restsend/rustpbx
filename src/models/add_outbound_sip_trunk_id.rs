use crate::models::call_record::Column as CallRecordColumn;
use crate::models::call_record::Entity as CallRecordEntity;
use sea_orm_migration::prelude::*;
use sea_query::ColumnDef;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_call_records";

        // Add outbound_sip_trunk_id column
        if !manager
            .has_column(table_name, "outbound_sip_trunk_id")
            .await?
        {
            manager
                .alter_table(
                    sea_query::Table::alter()
                        .table(CallRecordEntity)
                        .add_column(
                            ColumnDef::new(CallRecordColumn::OutboundSipTrunkId)
                                .big_integer()
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        // Add index for outbound_sip_trunk_id
        if !manager
            .has_index(table_name, "idx_call_records_outbound_sip_trunk_id")
            .await?
        {
            manager
                .create_index(
                    sea_query::Index::create()
                        .name("idx_call_records_outbound_sip_trunk_id")
                        .table(CallRecordEntity)
                        .col(CallRecordColumn::OutboundSipTrunkId)
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                sea_query::Index::drop()
                    .name("idx_call_records_outbound_sip_trunk_id")
                    .table(CallRecordEntity)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(CallRecordEntity)
                    .drop_column(CallRecordColumn::OutboundSipTrunkId)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}
