use anyhow::Result;
use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, pk_auto, string, string_null, string_uniq, timestamp, timestamp_null,
};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "rustpbx_users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub email: String,
    #[sea_orm(unique)]
    pub username: String,
    pub password_hash: String,
    pub reset_token: Option<String>,
    pub reset_token_expires: Option<DateTime>,
    pub last_login_at: Option<DateTime>,
    pub last_login_ip: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub is_active: bool,
    pub is_staff: bool,
    pub is_superuser: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    pub fn token_expired(&self) -> bool {
        match (self.reset_token.as_ref(), self.reset_token_expires) {
            (Some(_), Some(expiry)) => expiry < Utc::now().naive_utc(),
            _ => true,
        }
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
                    .col(pk_auto(Column::Id))
                    .col(string_uniq(Column::Email).char_len(255))
                    .col(string_uniq(Column::Username).char_len(100))
                    .col(string(Column::PasswordHash).char_len(255))
                    .col(string_null(Column::ResetToken).char_len(128))
                    .col(timestamp_null(Column::ResetTokenExpires))
                    .col(timestamp_null(Column::LastLoginAt))
                    .col(string_null(Column::LastLoginIp).char_len(128))
                    .col(timestamp(Column::CreatedAt))
                    .col(timestamp(Column::UpdatedAt))
                    .col(boolean(Column::IsActive).default(true))
                    .col(boolean(Column::IsStaff).default(false))
                    .col(boolean(Column::IsSuperuser).default(false))
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
