use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue::Set, ConnectionTrait, DbErr, QueryFilter};
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, integer_null, string, string_null, text_null, timestamp, timestamp_null,
};
use sea_orm_migration::sea_query::ColumnDef;
use sea_query::Expr;
use serde::Serialize;

pub const DEFAULT_FORWARDING_TIMEOUT: i32 = 30;
pub const MIN_FORWARDING_TIMEOUT: i32 = 5;
pub const MAX_FORWARDING_TIMEOUT: i32 = 120;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Default)]
#[sea_orm(table_name = "rustpbx_extensions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub extension: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub status: Option<String>,
    pub login_disabled: bool,
    pub voicemail_disabled: bool,
    pub allow_guest_calls: bool,
    pub sip_password: Option<String>,
    pub call_forwarding_mode: Option<String>,
    pub call_forwarding_destination: Option<String>,
    pub call_forwarding_timeout: Option<i32>,
    #[sea_orm(column_type = "DateTime")]
    pub registered_at: Option<DateTimeUtc>,
    pub notes: Option<String>,
    #[sea_orm(column_type = "DateTime", default_value = "CURRENT_TIMESTAMP")]
    pub created_at: DateTimeUtc,
    #[sea_orm(column_type = "DateTime", default_value = "CURRENT_TIMESTAMP")]
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter)]
pub enum Relation {}

impl RelationTrait for Relation {
    fn def(&self) -> RelationDef {
        panic!("no direct relations defined for extension");
    }
}

impl Related<super::department::Entity> for Entity {
    fn to() -> RelationDef {
        super::extension_department::Relation::Department.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::extension_department::Relation::Extension.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    pub async fn find_by_id_with_departments<C>(
        conn: &C,
        id: i64,
    ) -> Result<Option<(Model, Vec<super::department::Model>)>, DbErr>
    where
        C: ConnectionTrait,
    {
        let mut results = Self::find()
            .filter(Column::Id.eq(id))
            .find_with_related(super::department::Entity)
            .all(conn)
            .await?;

        Ok(results.pop())
    }

    pub async fn replace_departments<C>(
        conn: &C,
        extension_id: i64,
        department_ids: &[i64],
    ) -> Result<(), DbErr>
    where
        C: ConnectionTrait,
    {
        super::extension_department::Entity::delete_many()
            .filter(super::extension_department::Column::ExtensionId.eq(extension_id))
            .exec(conn)
            .await?;

        if department_ids.is_empty() {
            return Ok(());
        }

        let models =
            department_ids
                .iter()
                .map(|department_id| super::extension_department::ActiveModel {
                    extension_id: Set(extension_id),
                    department_id: Set(*department_id),
                    ..Default::default()
                });

        super::extension_department::Entity::insert_many(models)
            .exec(conn)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{department, migration::Migrator};
    use sea_orm::Database;

    #[tokio::test]
    async fn extension_can_map_to_multiple_departments() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");

        Migrator::up(&db, None)
            .await
            .expect("migrations should succeed");

        let extension = ActiveModel {
            extension: Set("1001".to_string()),
            login_disabled: Set(false),
            voicemail_disabled: Set(false),
            allow_guest_calls: Set(false),
            call_forwarding_mode: Set(Some("none".to_string())),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert extension");

        let sales = department::ActiveModel {
            name: Set("Sales".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert sales");

        let support = department::ActiveModel {
            name: Set("Support".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert support");

        Entity::replace_departments(&db, extension.id, &[sales.id, support.id])
            .await
            .expect("assign departments");

        let result = Entity::find_by_id_with_departments(&db, extension.id)
            .await
            .expect("query extension")
            .expect("extension exists");

        assert_eq!(result.1.len(), 2, "extension should have two departments");

        Entity::replace_departments(&db, extension.id, &[sales.id])
            .await
            .expect("reassign departments");

        let result = Entity::find_by_id_with_departments(&db, extension.id)
            .await
            .expect("query extension")
            .expect("extension exists");

        assert_eq!(result.1.len(), 1, "extension should have one department");
        assert_eq!(result.1[0].id, sales.id);
    }
}

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
                    .col(
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Column::Extension).char_len(32))
                    .col(string_null(Column::DisplayName).char_len(160))
                    .col(string_null(Column::Email).char_len(160))
                    .col(string_null(Column::Status).char_len(32))
                    .col(boolean(Column::LoginDisabled).default(false))
                    .col(boolean(Column::VoicemailDisabled).default(false))
                    .col(boolean(Column::AllowGuestCalls).default(false))
                    .col(string_null(Column::SipPassword).char_len(160))
                    .col(string_null(Column::CallForwardingMode).char_len(32))
                    .col(string_null(Column::CallForwardingDestination).char_len(160))
                    .col(integer_null(Column::CallForwardingTimeout))
                    .col(timestamp_null(Column::RegisteredAt))
                    .col(text_null(Column::Notes))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_rustpbx_extensions_extension")
                    .table(Entity)
                    .col(Column::Extension)
                    .unique()
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}
