use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{boolean, json_null, string, text_null, timestamp, timestamp_null};
use sea_orm_migration::sea_query::ColumnDef;
use sea_query::Expr;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_queues")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub description: Option<String>,
    pub metadata: Option<Json>,
    pub spec: Json,
    pub is_active: bool,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub last_exported_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "queue"
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
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .primary_key()
                            .auto_increment(),
                    )
                    .col(string(Column::Name).char_len(160))
                    .col(text_null(Column::Description))
                    .col(json_null(Column::Metadata))
                    .col(ColumnDef::new(Column::Spec).json().not_null())
                    .col(boolean(Column::IsActive).default(true))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .col(timestamp_null(Column::LastExportedAt))
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
