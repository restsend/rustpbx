use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::timestamp;
use sea_orm_migration::sea_query::{
    ColumnDef, ForeignKeyAction as MigrationForeignKeyAction, Index,
};
use sea_query::Expr;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "rustpbx_extension_departments")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub extension_id: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub department_id: i64,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::extension::Entity",
        from = "Column::ExtensionId",
        to = "super::extension::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Extension,
    #[sea_orm(
        belongs_to = "super::department::Entity",
        from = "Column::DepartmentId",
        to = "super::department::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Department,
}

impl Related<super::extension::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Extension.def()
    }
}

impl Related<super::department::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Department.def()
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
                    .col(ColumnDef::new(Column::ExtensionId).big_integer().not_null())
                    .col(
                        ColumnDef::new(Column::DepartmentId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .primary_key(
                        Index::create()
                            .col(Column::ExtensionId)
                            .col(Column::DepartmentId),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_extension_department_extension")
                            .from(Entity, Column::ExtensionId)
                            .to(super::extension::Entity, super::extension::Column::Id)
                            .on_delete(MigrationForeignKeyAction::Cascade)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_extension_department_department")
                            .from(Entity, Column::DepartmentId)
                            .to(super::department::Entity, super::department::Column::Id)
                            .on_delete(MigrationForeignKeyAction::Cascade)
                            .on_update(MigrationForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("uniq_rustpbx_extension_departments_pair")
                    .table(Entity)
                    .col(Column::ExtensionId)
                    .col(Column::DepartmentId)
                    .unique()
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
