//! Report columns for `rustpbx_call_records`: hangup_reason + sip_status_code.
//!
//! Both values previously lived only inside the `metadata` JSON (and the
//! JSONL CDR file), so reports grouping by hangup cause / SIP response code
//! required per-row JSON extraction. Promoting them to real columns lets the
//! report layer aggregate with plain GROUP BY over indexed-scan-friendly
//! queries.

use super::call_record::Column as CallRecordColumn;
use super::call_record::Entity as CallRecordEntity;
use sea_orm_migration::prelude::*;
use sea_query::ColumnDef;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_call_records";

        if !manager
            .has_column(table_name, "hangup_reason")
            .await?
        {
            manager
                .alter_table(
                    sea_query::Table::alter()
                        .table(CallRecordEntity)
                        .add_column(
                            ColumnDef::new(CallRecordColumn::HangupReason)
                                .string_len(64)
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_column(table_name, "sip_status_code")
            .await?
        {
            manager
                .alter_table(
                    sea_query::Table::alter()
                        .table(CallRecordEntity)
                        .add_column(
                            ColumnDef::new(CallRecordColumn::SipStatusCode)
                                .integer()
                                .null(),
                        )
                        .to_owned(),
                )
                .await?;
        }

        if !manager
            .has_index(table_name, "idx_call_records_hangup_reason")
            .await?
        {
            manager
                .create_index(
                    sea_query::Index::create()
                        .name("idx_call_records_hangup_reason")
                        .table(CallRecordEntity)
                        .col(CallRecordColumn::HangupReason)
                        .if_not_exists()
                        .to_owned(),
                )
                .await?;
        }

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
