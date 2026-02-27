use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::{ColumnDef as MigrationColumnDef, *};
use serde::{Deserialize, Serialize};

/// A system-generated notification stored in the database.
/// Kinds: "info" | "warning" | "update"
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "system_notifications")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Notification kind: "info", "warning", "update"
    pub kind: String,
    pub title: String,
    pub body: String,
    pub read: bool,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

// ─── Migration ──────────────────────────────────────────────────────────────

pub struct Migration;

impl MigrationName for Migration {
    fn name(&self) -> &str {
        "m20260227_000001_create_system_notifications"
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
                        MigrationColumnDef::new(Column::Kind)
                            .string()
                            .string_len(32)
                            .not_null(),
                    )
                    .col(
                        MigrationColumnDef::new(Column::Title)
                            .string()
                            .string_len(255)
                            .not_null(),
                    )
                    .col(MigrationColumnDef::new(Column::Body).text().not_null())
                    .col(
                        MigrationColumnDef::new(Column::Read)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        MigrationColumnDef::new(Column::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
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
