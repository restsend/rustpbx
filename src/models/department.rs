use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{json_null, pk_auto, string, string_null, text_null, timestamp};
use sea_query::Expr;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "rustpbx_departments")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub display_label: Option<String>,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub manager_contact: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub metadata: Option<Json>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::extension_department::Entity")]
    ExtensionDepartment,
}

impl Related<super::extension::Entity> for Entity {
    fn to() -> RelationDef {
        super::extension_department::Relation::Extension.def()
    }

    fn via() -> Option<RelationDef> {
        Some(
            super::extension_department::Relation::Department
                .def()
                .rev(),
        )
    }
}

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
                    .col(pk_auto(Column::Id))
                    .col(string(Column::Name).char_len(120))
                    .col(string_null(Column::DisplayLabel).char_len(160))
                    .col(string_null(Column::Slug).char_len(120))
                    .col(text_null(Column::Description))
                    .col(string_null(Column::Color).char_len(32))
                    .col(string_null(Column::ManagerContact).char_len(160))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .col(json_null(Column::Metadata))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .table(Entity)
                    .name("idx_rustpbx_departments_slug")
                    .col(Column::Slug)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_rustpbx_departments_slug")
                    .table(Entity)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}
