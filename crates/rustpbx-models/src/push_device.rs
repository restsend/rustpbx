use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::*;
use serde::{Deserialize, Serialize};

/// RFC 8599 push-capable device registration.
///
/// One row per (identity, device). Written by the registrar when a REGISTER
/// Contact carries `pn-provider`/`pn-prid`, so a device that has closed its
/// connection (e.g. a mobile app killed by the OS) can still be woken by a
/// push notification for an incoming call — including after a PBX restart.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_push_devices")]
pub struct Model {
    /// Locator identifier of the registered user (`username@realm`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub identity: String,
    /// `instance:<+sip.instance>` or `prid:<sha256(provider:prid)>`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub device_key: String,
    pub username: String,
    pub realm: Option<String>,
    /// Registered address-of-record (REGISTER To URI).
    pub aor: String,
    /// Last registered Contact URI (carries the `pn-*` parameters).
    pub contact: String,
    pub instance_id: Option<String>,
    pub provider: String,
    pub prid: String,
    pub param: Option<String>,
    pub user_agent: Option<String>,
    /// Unix seconds when the binding expires unless refreshed.
    pub expires_at: i64,
    /// Unix seconds of the last REGISTER that created/refreshed the binding.
    pub registered_at: i64,
    /// Unix seconds of the binding-refresh push sent for the current
    /// registration (RFC 8599 §5.5); reset by every REGISTER.
    pub refresh_pushed_at: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

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
                    .col(string(Column::Identity).string_len(255))
                    .col(string(Column::DeviceKey).string_len(255))
                    .col(string(Column::Username).string_len(200))
                    .col(string_null(Column::Realm).string_len(200))
                    .col(text(Column::Aor))
                    .col(text(Column::Contact))
                    .col(string_null(Column::InstanceId).string_len(255))
                    .col(string(Column::Provider).string_len(32))
                    .col(text(Column::Prid))
                    .col(text_null(Column::Param))
                    .col(text_null(Column::UserAgent))
                    .col(big_integer(Column::ExpiresAt))
                    .col(big_integer(Column::RegisteredAt))
                    .col(big_integer_null(Column::RefreshPushedAt))
                    .primary_key(Index::create().col(Column::Identity).col(Column::DeviceKey))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .table(Entity)
                    .name("idx_push_devices_expires_at")
                    .col(Column::ExpiresAt)
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
