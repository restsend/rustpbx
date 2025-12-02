use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::{ColumnDef as MigrationColumnDef, *};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize)]
#[sea_orm(table_name = "rustpbx_frequency_limits")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub policy_id: String,
    #[sea_orm(indexed)]
    pub scope: String,
    #[sea_orm(indexed)]
    pub scope_value: String,
    pub limit_type: String, // "frequency", "daily", "concurrency"
    pub count: u32,
    pub window_end: Option<DateTimeUtc>,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m_20251126_000001_create_frequency_limits_table"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Entity)
                    .if_not_exists()
                    .col(
                        MigrationColumnDef::new(Column::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        MigrationColumnDef::new(Column::PolicyId)
                            .string()
                            .string_len(64)
                            .not_null(),
                    )
                    .col(
                        MigrationColumnDef::new(Column::Scope)
                            .string()
                            .string_len(32)
                            .not_null(),
                    )
                    .col(
                        MigrationColumnDef::new(Column::ScopeValue)
                            .string()
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        MigrationColumnDef::new(Column::LimitType)
                            .string()
                            .string_len(32)
                            .not_null(),
                    )
                    .col(MigrationColumnDef::new(Column::Count).unsigned().not_null())
                    .col(MigrationColumnDef::new(Column::WindowEnd).timestamp())
                    .col(
                        MigrationColumnDef::new(Column::UpdatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        // Create index for faster lookups
        manager
            .create_index(
                Index::create()
                    .name("idx_frequency_limits_lookup")
                    .table(Entity)
                    .col(Column::PolicyId)
                    .col(Column::Scope)
                    .col(Column::ScopeValue)
                    .col(Column::LimitType)
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
