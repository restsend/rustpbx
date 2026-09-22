//! Media-quality columns for `rustpbx_call_records`.
//!
//! Per-leg RTCP quality (loss %, jitter, RTT) already reaches the CDR as a
//! `metadata.media_quality` JSON array, but report aggregation needs plain
//! indexed-ready columns. The writer picks the trunk-facing leg (A for
//! inbound, B for outbound) and fills:
//!
//! - `media_loss_pct` — RTCP packet-loss percentage
//! - `media_jitter_ms` — RTCP jitter in milliseconds
//! - `media_rtt_ms` — RTCP round-trip time in milliseconds
//!
//! NULL when the call has no quality report (no bridge, no RTCP feedback).

use super::call_record::Entity as CallRecordEntity;
use sea_orm_migration::prelude::*;
use sea_query::ColumnDef;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let table_name = "rustpbx_call_records";

        for name in ["media_loss_pct", "media_jitter_ms", "media_rtt_ms"] {
            if !manager.has_column(table_name, name).await? {
                manager
                    .alter_table(
                        sea_query::Table::alter()
                            .table(CallRecordEntity)
                            .add_column(
                                ColumnDef::new(sea_query::Alias::new(name)).double().null(),
                            )
                            .to_owned(),
                    )
                    .await?;
            }
        }

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
