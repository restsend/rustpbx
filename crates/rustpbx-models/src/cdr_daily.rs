use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{big_integer, big_integer_null, double, string, timestamp};
use serde::{Deserialize, Serialize};

/// Pre-aggregated daily CDR rollup (`rustpbx_cdr_daily`).
///
/// One row per (day, direction, department, trunk) combo, recomputed
/// idempotently from `rustpbx_call_records` by `crate::report::rollup`.
/// Powers dashboard drill-down without scanning the raw CDR table.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_cdr_daily")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    /// UTC midnight of the covered day.
    pub day: DateTimeUtc,
    /// inbound / outbound / internal.
    pub direction: String,
    pub department_id: Option<i64>,
    pub sip_trunk_id: Option<i64>,
    pub total_calls: i64,
    pub answered: i64,
    pub missed: i64,
    pub total_duration_secs: i64,
    pub avg_duration_secs: f64,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Entity)
                    .if_not_exists()
                    .col(big_integer(Column::Id).primary_key().auto_increment())
                    .col(timestamp(Column::Day))
                    .col(string(Column::Direction).char_len(32))
                    .col(big_integer_null(Column::DepartmentId))
                    .col(big_integer_null(Column::SipTrunkId))
                    .col(big_integer(Column::TotalCalls).default(0))
                    .col(big_integer(Column::Answered).default(0))
                    .col(big_integer(Column::Missed).default(0))
                    .col(big_integer(Column::TotalDurationSecs).default(0))
                    .col(double(Column::AvgDurationSecs).default(0.0))
                    .col(timestamp(Column::UpdatedAt))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .table(Entity)
                    .name("idx_cdr_daily_day_dims")
                    .col(Column::Day)
                    .col(Column::Direction)
                    .col(Column::DepartmentId)
                    .col(Column::SipTrunkId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}
