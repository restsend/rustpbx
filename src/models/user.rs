use anyhow::Context;
use anyhow::{Result, ensure};
use argon2::Argon2;
use argon2::PasswordHasher;
use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    boolean, string, string_null, string_uniq, timestamp, timestamp_null,
};
use sea_orm_migration::sea_query::ColumnDef;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize)]
#[sea_orm(table_name = "rustpbx_users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub email: String,
    #[sea_orm(unique)]
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    #[serde(skip_serializing)]
    pub reset_token: Option<String>,
    #[serde(skip_serializing)]
    pub reset_token_expires: Option<DateTimeUtc>,
    pub last_login_at: Option<DateTimeUtc>,
    pub last_login_ip: Option<String>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
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
            (Some(_), Some(expiry)) => expiry < Utc::now(),
            _ => true,
        }
    }

    pub async fn upsert_super_user(
        db: &DatabaseConnection,
        username: &str,
        email: &str,
        password: &str,
    ) -> Result<Model> {
        let username = username.trim();
        let email = email.trim().to_lowercase();
        ensure!(!username.is_empty(), "username is required");
        ensure!(!email.is_empty(), "email is required");
        ensure!(!password.is_empty(), "password is required");

        let salt = SaltString::generate(&mut OsRng);
        let hashed = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("failed to hash password: {}", e))?
            .to_string();

        let now = Utc::now();

        let mut user = Entity::find()
            .filter(Column::Username.eq(username))
            .one(db)
            .await
            .with_context(|| format!("failed to lookup user by username: {}", username))?;

        if user.is_none() {
            if let Some(existing) = Entity::find()
                .filter(Column::Email.eq(email.clone()))
                .one(db)
                .await
                .with_context(|| format!("failed to lookup user by email: {}", email))?
            {
                user = Some(existing);
            }
        }

        if let Some(user) = user {
            let mut model: ActiveModel = user.into();
            model.username = Set(username.to_string());
            model.email = Set(email.clone());
            model.password_hash = Set(hashed);
            model.is_active = Set(true);
            model.is_staff = Set(true);
            model.is_superuser = Set(true);
            model.reset_token = Set(None);
            model.reset_token_expires = Set(None);
            model.updated_at = Set(now);
            model
                .update(db)
                .await
                .context("failed to update super user")
        } else {
            let mut model: ActiveModel = Default::default();
            model.username = Set(username.to_string());
            model.email = Set(email.clone());
            model.password_hash = Set(hashed);
            model.created_at = Set(now);
            model.updated_at = Set(now);
            model.is_active = Set(true);
            model.is_staff = Set(true);
            model.is_superuser = Set(true);
            model.reset_token = Set(None);
            model.reset_token_expires = Set(None);
            model
                .insert(db)
                .await
                .context("failed to create super user")
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
                    .col(
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string_uniq(Column::Email).char_len(255))
                    .col(string_uniq(Column::Username).char_len(100))
                    .col(string(Column::PasswordHash).char_len(255))
                    .col(string_null(Column::ResetToken).char_len(128))
                    .col(timestamp_null(Column::ResetTokenExpires))
                    .col(timestamp_null(Column::LastLoginAt))
                    .col(string_null(Column::LastLoginIp).char_len(128))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
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
