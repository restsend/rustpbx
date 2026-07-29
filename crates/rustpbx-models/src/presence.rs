use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "presence_states")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub identity: String,
    pub status: String,
    pub note: Option<String>,
    pub activity: Option<String>,
    pub last_updated: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

#[derive(sea_orm_migration::prelude::DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl sea_orm_migration::MigrationTrait for Migration {
    async fn up(&self, manager: &sea_orm_migration::SchemaManager) -> Result<(), sea_orm::DbErr> {
        use sea_orm_migration::prelude::*;
        manager
            .create_table(
                Table::create()
                    .table(Entity)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Column::Identity)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Column::Status).string().not_null())
                    .col(ColumnDef::new(Column::Note).string())
                    .col(ColumnDef::new(Column::Activity).string())
                    .col(ColumnDef::new(Column::LastUpdated).big_integer().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &sea_orm_migration::SchemaManager) -> Result<(), sea_orm::DbErr> {
        use sea_orm_migration::prelude::*;
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}
