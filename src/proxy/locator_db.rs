use super::locator::{
    Locator, RealmChecker, is_local_realm, sort_locations_by_recency, uri_matches,
};
use crate::call::Location;
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transport::SipAddr;
use sea_orm::{ActiveModelTrait, Database, QueryOrder, Set, entity::prelude::*};
pub use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{boolean, integer, pk_auto, string, string_null, timestamp};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
// ... (rest of the model)
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
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
    pub supports_webrtc: bool,
    pub user_agent: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
impl Entity {}

/// Database backed Locator implementation using SeaORM
pub struct DbLocator {
    db: DatabaseConnection,
    realm_checker: Mutex<Option<RealmChecker>>,
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
                    .col(string_null(Column::Realm).char_len(200))
                    .col(string(Column::Destination).char_len(255).not_null())
                    .col(string(Column::Transport).char_len(32).not_null())
                    .col(integer(Column::LastModified).not_null())
                    .col(timestamp(Column::CreatedAt).not_null())
                    .col(timestamp(Column::UpdatedAt).not_null())
                    .col(boolean(Column::SupportsWebrtc).not_null().default(false))
                    .col(string_null(Column::UserAgent).char_len(255))
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
        let db_locator = Self {
            db,
            realm_checker: Mutex::new(None),
        };
        info!("Creating DbLocator");
        match db_locator.migrate().await {
            Ok(_) => Ok(db_locator),
            Err(e) => {
                warn!("migrate locator fail {}", e);
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
    async fn is_local_realm(&self, realm: &str) -> bool {
        let checker = self.realm_checker.lock().await.clone();
        if let Some(checker) = checker {
            checker(realm).await
        } else {
            is_local_realm(realm)
        }
    }

    fn set_realm_checker(&self, checker: RealmChecker) {
        let mut lock = self
            .realm_checker
            .try_lock()
            .expect("failed to lock realm_checker");
        *lock = Some(checker);
    }

    async fn register(
        &self,
        username: &str,
        realm: Option<&str>,
        location: Location,
    ) -> Result<()> {
        // Default implementation for standard cases:
        let aor = location.aor.to_string();
        let expires = location.expires as i64;
        let username_key = username.trim().to_ascii_lowercase();
        if username_key.is_empty() {
            return Err(anyhow::anyhow!("Cannot register location without username"));
        }

        let realm_key = match realm {
            Some(r) if !r.trim().is_empty() => {
                let r = r.trim();
                if self.is_local_realm(r).await {
                    "localhost".to_string()
                } else {
                    r.to_ascii_lowercase()
                }
            }
            _ => String::new(),
        };
        let destination = match &location.destination {
            Some(dest) => dest,
            None => {
                return Err(anyhow::anyhow!(
                    "Cannot register location without destination"
                ));
            }
        };
        // Extract SipAddr components
        let SipAddr { r#type, addr } = destination;
        let transport = location
            .transport
            .or_else(|| r#type.clone())
            .unwrap_or(rsip::transport::Transport::Udp);
        let host = addr.to_string();

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if expires <= 0 {
            Entity::delete_many()
                .filter(Column::Username.eq(&username_key))
                .filter(Column::Realm.eq(&realm_key))
                .filter(Column::Aor.eq(&aor))
                .exec(&self.db)
                .await
                .map_err(|e| anyhow::anyhow!("Database error on register delete: {}", e))?;
            return Ok(());
        }

        // Check if record exists
        let existing = Entity::find()
            .filter(Column::Username.eq(&username_key))
            .filter(Column::Realm.eq(&realm_key))
            .filter(Column::Aor.eq(&aor))
            .one(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on register lookup: {}", e))?;

        match existing {
            Some(model) => {
                // Update existing record
                let mut active_model: ActiveModel = model.into();
                active_model.expires = Set(expires);
                active_model.username = Set(username_key.clone());
                active_model.realm = Set(realm_key.clone());
                active_model.destination = Set(host);
                active_model.aor = Set(aor);
                active_model.transport = Set(transport.to_string());
                active_model.last_modified = Set(now);
                active_model.updated_at = Set(chrono::Utc::now());
                active_model.supports_webrtc = Set(location.supports_webrtc);
                active_model.user_agent = Set(location.user_agent.clone());

                active_model
                    .update(&self.db)
                    .await
                    .map_err(|e| anyhow::anyhow!("Database error on register update: {}", e))?;
            }
            None => {
                // Create new record - let the database assign the ID
                let now_dt = chrono::Utc::now();

                // Create a new active model with all fields except id
                let mut active_model = ActiveModel::new();
                active_model.aor = Set(aor);
                active_model.expires = Set(expires);
                active_model.username = Set(username_key);
                active_model.realm = Set(realm_key);
                active_model.destination = Set(host);
                active_model.transport = Set(transport.to_string());
                active_model.last_modified = Set(now);
                active_model.created_at = Set(now_dt);
                active_model.updated_at = Set(now_dt);
                active_model.supports_webrtc = Set(location.supports_webrtc);
                active_model.user_agent = Set(location.user_agent.clone());

                // Insert without specifying id
                active_model
                    .insert(&self.db)
                    .await
                    .map_err(|e| anyhow::anyhow!("Database error on register insert: {}", e))?;
            }
        }

        Ok(())
    }

    async fn unregister_with_address(&self, addr: &SipAddr) -> Result<Option<Vec<Location>>> {
        // Unregister all locations matching the given address
        let host = addr.addr.to_string();
        let transport = addr
            .r#type
            .map(|t| t.to_string())
            .unwrap_or_else(|| "UDP".to_string());
        let removed_locations = Entity::find()
            .filter(Column::Destination.eq(&host))
            .filter(Column::Transport.eq(&transport))
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on lookup before unregister: {}", e))?;
        if removed_locations.is_empty() {
            return Ok(None);
        }

        Entity::delete_many()
            .filter(Column::Destination.eq(&host))
            .filter(Column::Transport.eq(&transport))
            .exec(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on unregister with address: {}", e))?;

        let mut locations = Vec::new();
        for loc in removed_locations {
            let aor = rsip::Uri::try_from(loc.aor.as_str())
                .map_err(|e| anyhow::anyhow!("Error parsing aor: {}", e))?;
            let registered_aor = aor.clone();
            // Parse transport from string
            let transport = match loc.transport.to_uppercase().as_str() {
                "UDP" => rsip::transport::Transport::Udp,
                "TCP" => rsip::transport::Transport::Tcp,
                "TLS" => rsip::transport::Transport::Tls,
                "WS" => rsip::transport::Transport::Ws,
                "WSS" => rsip::transport::Transport::Wss,
                _ => rsip::transport::Transport::Udp, // Default to UDP
            };

            // Parse destination host to HostWithPort
            let addr = loc.destination.try_into()?;

            // Create SipAddr
            let destination = SipAddr {
                r#type: Some(transport),
                addr,
            };

            locations.push(Location {
                aor,
                expires: loc.expires as u32,
                destination: Some(destination),
                supports_webrtc: loc.supports_webrtc,
                transport: Some(transport),
                registered_aor: Some(registered_aor),
                user_agent: loc.user_agent.clone(),
                ..Default::default()
            });
        }
        Ok(Some(sort_locations_by_recency(locations)))
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        // Standard implementation for other cases
        let username_key = username.trim().to_ascii_lowercase();
        if username_key.is_empty() {
            return Ok(());
        }

        let realm_key = match realm {
            Some(r) if !r.trim().is_empty() => {
                let r = r.trim();
                if self.is_local_realm(r).await {
                    "localhost".to_string()
                } else {
                    r.to_ascii_lowercase()
                }
            }
            _ => String::new(),
        };

        Entity::delete_many()
            .filter(Column::Username.eq(username_key))
            .filter(Column::Realm.eq(realm_key))
            .exec(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on unregister: {}", e))?;

        Ok(())
    }

    async fn lookup(&self, uri: &rsip::Uri) -> Result<Vec<Location>> {
        let now_epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let target_aor = uri.to_string();
        let mut models = Entity::find()
            .filter(Column::Aor.eq(&target_aor))
            .order_by_desc(Column::LastModified)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on lookup by aor: {}", e))?;

        if models.is_empty() && uri.host().to_string().ends_with(".invalid") {
            models = Entity::find()
                .filter(Column::Aor.contains(uri.host().to_string().as_str()))
                .order_by_desc(Column::LastModified)
                .all(&self.db)
                .await
                .map_err(|e| anyhow::anyhow!("Database error on lookup by invalid host: {}", e))?;
        }

        if models.is_empty() {
            let realm_raw = uri.host().to_string();
            let mut realm_key = realm_raw.trim().to_ascii_lowercase();
            if self.is_local_realm(&realm_key).await {
                realm_key = "localhost".to_string();
            }
            let username_raw = uri.user().unwrap_or_else(|| "");
            let username_trimmed = username_raw.trim();
            let username_key = username_trimmed.to_ascii_lowercase();

            if !username_key.is_empty() {
                models = Entity::find()
                    .filter(Column::Username.eq(&username_key))
                    .filter(Column::Realm.eq(&realm_key))
                    .order_by_desc(Column::LastModified)
                    .all(&self.db)
                    .await
                    .map_err(|e| anyhow::anyhow!("Database error on lookup: {}", e))?;

                if models.is_empty()
                    && (realm_key.is_empty() || self.is_local_realm(&realm_key).await)
                {
                    models = Entity::find()
                        .filter(Column::Username.eq(&username_key))
                        .order_by_desc(Column::LastModified)
                        .all(&self.db)
                        .await
                        .map_err(|e| anyhow::anyhow!("Database error on username lookup: {}", e))?;
                }
            }
        }

        if models.is_empty() {
            return Ok(vec![]);
        }

        let mut locations = Vec::new();
        let now_instant = Instant::now();
        for model in models {
            if model.expires > 0 {
                let elapsed = now_epoch - model.last_modified;
                if elapsed >= model.expires {
                    continue;
                }
            }
            let aor = rsip::Uri::try_from(model.aor.as_str())
                .map_err(|e| anyhow::anyhow!("Error parsing aor: {}", e))?;

            if !uri_matches(&aor, uri) {
                continue;
            }

            let registered_aor = aor.clone();
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

            let age_secs = if model.last_modified >= now_epoch {
                0
            } else {
                (now_epoch - model.last_modified) as u64
            };
            let age_duration = Duration::from_secs(age_secs);
            let last_modified_instant =
                now_instant.checked_sub(age_duration).unwrap_or(now_instant);

            locations.push(Location {
                aor,
                expires: model.expires as u32,
                destination: Some(destination),
                last_modified: Some(last_modified_instant),
                supports_webrtc: model.supports_webrtc,
                transport: Some(transport),
                registered_aor: Some(registered_aor),
                user_agent: model.user_agent.clone(),
                ..Default::default()
            });
        }

        Ok(sort_locations_by_recency(locations))
    }
}
