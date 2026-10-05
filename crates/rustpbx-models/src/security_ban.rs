use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::{ColumnDef as MigrationColumnDef, *};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize)]
#[sea_orm(table_name = "rustpbx_banned_ips")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub ip: String,
    #[sea_orm(indexed)]
    pub reason: String,
    pub last_username: Option<String>,
    pub last_method: Option<String>,
    pub offense_count: i32,
    #[sea_orm(indexed)]
    pub banned_until: i64,
    pub created_at: i64,
    pub released_at: Option<i64>,
    pub released_by: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m_20261002_000001_create_banned_ips_table"
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
                        MigrationColumnDef::new(Column::Ip)
                            .string()
                            .string_len(64)
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        MigrationColumnDef::new(Column::Reason)
                            .string()
                            .string_len(64)
                            .not_null(),
                    )
                    .col(MigrationColumnDef::new(Column::LastUsername).string().string_len(128).null())
                    .col(MigrationColumnDef::new(Column::LastMethod).string().string_len(32).null())
                    .col(MigrationColumnDef::new(Column::OffenseCount).integer().not_null().default(1))
                    .col(MigrationColumnDef::new(Column::BannedUntil).big_integer().not_null())
                    .col(MigrationColumnDef::new(Column::CreatedAt).big_integer().not_null())
                    .col(MigrationColumnDef::new(Column::ReleasedAt).big_integer().null())
                    .col(MigrationColumnDef::new(Column::ReleasedBy).string().string_len(128).null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.drop_table(Table::drop().table(Entity).to_owned()).await
    }
}
