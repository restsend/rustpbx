use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, integer, json_null, pk_auto, string, string_null, text_null, timestamp, timestamp_null,
};
use sea_query::Expr;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "rustpbx_extensions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub extension: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub status: Option<String>,
    pub login_allowed: bool,
    pub voicemail_enabled: bool,
    pub sip_password: Option<String>,
    pub pin: Option<String>,
    pub caller_id_name: Option<String>,
    pub caller_id_number: Option<String>,
    pub outbound_caller_id: Option<String>,
    pub emergency_caller_id: Option<String>,
    pub call_forwarding_mode: String,
    pub call_forwarding_destination: Option<String>,
    pub call_forwarding_timeout: Option<i32>,
    pub registrar: Option<String>,
    pub registered_at: Option<DateTime>,
    pub metadata: Option<Json>,
    pub notes: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::extension_department::Entity")]
    ExtensionDepartment,
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
                    .col(string(Column::Extension).char_len(32))
                    .col(string_null(Column::DisplayName).char_len(160))
                    .col(string_null(Column::Email).char_len(160))
                    .col(string_null(Column::Status).char_len(32))
                    .col(boolean(Column::LoginAllowed).default(true))
                    .col(boolean(Column::VoicemailEnabled).default(false))
                    .col(string_null(Column::SipPassword).char_len(160))
                    .col(string_null(Column::Pin).char_len(32))
                    .col(string_null(Column::CallerIdName).char_len(160))
                    .col(string_null(Column::CallerIdNumber).char_len(64))
                    .col(string_null(Column::OutboundCallerId).char_len(64))
                    .col(string_null(Column::EmergencyCallerId).char_len(64))
                    .col(
                        string(Column::CallForwardingMode)
                            .char_len(32)
                            .default("none"),
                    )
                    .col(string_null(Column::CallForwardingDestination).char_len(160))
                    .col(integer(Column::CallForwardingTimeout).null())
                    .col(string_null(Column::Registrar).char_len(160))
                    .col(timestamp_null(Column::RegisteredAt))
                    .col(json_null(Column::Metadata))
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
