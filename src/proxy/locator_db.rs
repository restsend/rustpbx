use super::locator::{Location as LocatorLocation, Locator};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transport::SipAddr;
use sea_orm::{entity::prelude::*, ActiveModelTrait, Database, Set};
pub use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{boolean, integer, pk_auto, string, timestamp};
use std::time::{Instant, SystemTime};
use tracing::{error, info};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "rustpbx_locations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    pub aor: String,
    pub expires: i64,
    pub username: String,
    pub realm: String,
    pub destination: String,
    pub transport: String,
    pub last_modified: i64,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub supports_webrtc: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
impl Entity {}

/// Database backed Locator implementation using SeaORM
pub struct DbLocator {
    db: DatabaseConnection,
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
                    .col(string(Column::Aor).char_len(255).not_null())
                    .col(integer(Column::Expires).not_null())
                    .col(string(Column::Username).char_len(200).not_null())
                    .col(string(Column::Realm).char_len(200).null())
                    .col(string(Column::Destination).char_len(255).not_null())
                    .col(string(Column::Transport).char_len(32).not_null())
                    .col(integer(Column::LastModified).not_null())
                    .col(timestamp(Column::CreatedAt).not_null())
                    .col(timestamp(Column::UpdatedAt).not_null())
                    .col(boolean(Column::SupportsWebrtc).not_null().default(false))
                    .to_owned(),
            )
            .await?;

        // Add index on username+realm
        manager
            .create_index(
                Index::create()
                    .table(Entity)
                    .name("idx_locations_realm_username")
                    .col(Column::Realm)
                    .col(Column::Username)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .table(Entity)
                    .name("idx_locations_realm_username")
                    .to_owned(),
            )
            .await?;

        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(Migration {})]
    }
}

impl DbLocator {
    /// Create a new DbLocator with a database connection
    pub async fn new(url: String) -> Result<Self> {
        // Connect to the database
        let db = Database::connect(&url)
            .await
            .map_err(|e| anyhow::anyhow!("Database connection error: {}", e))?;
        let db_locator = Self { db };
        info!("Creating DbLocator");
        match db_locator.migrate().await {
            Ok(_) => Ok(db_locator),
            Err(e) => {
                error!("migrate locator fail {}", e);
                Err(e)
            }
        }
    }

    pub async fn migrate(&self) -> Result<()> {
        Migrator::up(&self.db, None)
            .await
            .map_err(|e| anyhow::anyhow!("Migration error: {}", e))?;
        Ok(())
    }
}

#[async_trait]
impl Locator for DbLocator {
    async fn register(
        &self,
        username: &str,
        realm: Option<&str>,
        location: LocatorLocation,
    ) -> Result<()> {
        // Default implementation for standard cases:
        let aor = location.aor.to_string();
        let expires = location.expires as i64;
        let realm_value = realm.unwrap_or_default().to_string();

        // Extract SipAddr components
        let (host, transport) = match &location.destination {
            SipAddr { r#type, addr } => {
                let transport = match r#type {
                    Some(t) => t.to_string(),
                    None => "UDP".to_string(), // Default to UDP if not specified
                };
                (addr.to_string(), transport)
            }
        };

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Check if record exists
        let existing = Entity::find()
            .filter(Column::Username.eq(username))
            .filter(Column::Realm.eq(&realm_value))
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on register lookup: {}", e))?;

        match existing {
            Some(model) => {
                // Update existing record
                let mut active_model: ActiveModel = model.into();
                active_model.expires = Set(expires);
                active_model.username = Set(username.to_string());
                active_model.realm = Set(realm_value);
                active_model.destination = Set(host);
                active_model.aor = Set(aor);
                active_model.transport = Set(transport);
                active_model.last_modified = Set(now);
                active_model.updated_at = Set(chrono::Utc::now().naive_utc());
                active_model.supports_webrtc = Set(location.supports_webrtc);

                active_model
                    .update(&self.db)
                    .await
                    .map_err(|e| anyhow::anyhow!("Database error on register update: {}", e))?;
            }
            None => {
                // Create new record - let the database assign the ID
                let now_dt = chrono::Utc::now().naive_utc();

                // Create a new active model with all fields except id
                let mut active_model = ActiveModel::new();
                active_model.aor = Set(aor);
                active_model.expires = Set(expires);
                active_model.username = Set(username.to_string());
                active_model.realm = Set(realm_value);
                active_model.destination = Set(host);
                active_model.transport = Set(transport);
                active_model.last_modified = Set(now);
                active_model.created_at = Set(now_dt);
                active_model.updated_at = Set(now_dt);
                active_model.supports_webrtc = Set(location.supports_webrtc);

                // Insert without specifying id
                active_model
                    .insert(&self.db)
                    .await
                    .map_err(|e| anyhow::anyhow!("Database error on register insert: {}", e))?;
            }
        }

        Ok(())
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        // Standard implementation for other cases
        let realm_value = realm.unwrap_or_default();

        Entity::delete_many()
            .filter(Column::Username.eq(username))
            .filter(Column::Realm.eq(realm_value))
            .exec(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on unregister: {}", e))?;

        Ok(())
    }

    async fn lookup(&self, username: &str, realm: Option<&str>) -> Result<Vec<LocatorLocation>> {
        // Default implementation for standard cases
        let realm_value = realm.unwrap_or_default();
        let models = Entity::find()
            .filter(Column::Username.eq(username))
            .filter(Column::Realm.eq(realm_value))
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on lookup: {}", e))?;

        if models.is_empty() {
            return Err(anyhow::anyhow!("missing user: {}", username));
        }

        let mut locations = Vec::new();
        for model in models {
            // Parse the aor into a Uri
            let aor = rsip::Uri::try_from(model.aor.as_str())
                .map_err(|e| anyhow::anyhow!("Error parsing aor: {}", e))?;

            // Parse transport from string
            let transport = match model.transport.to_uppercase().as_str() {
                "UDP" => rsip::transport::Transport::Udp,
                "TCP" => rsip::transport::Transport::Tcp,
                "TLS" => rsip::transport::Transport::Tls,
                "WS" => rsip::transport::Transport::Ws,
                "WSS" => rsip::transport::Transport::Wss,
                _ => rsip::transport::Transport::Udp, // Default to UDP
            };

            // Parse destination host to HostWithPort
            let addr = model.destination.try_into()?;

            // Create SipAddr
            let destination = SipAddr {
                r#type: Some(transport),
                addr,
            };

            locations.push(LocatorLocation {
                aor,
                expires: model.expires as u32,
                destination,
                last_modified: Instant::now(), // We can't store Instant in DB, so create a new one
                supports_webrtc: model.supports_webrtc,
            });
        }

        Ok(locations)
    }
}
