use super::locator::{
    Locator, RealmChecker, UNREGISTER_GRACE_SECS, choose_registered_aor, invalid_host_fallback,
    is_local_realm, is_location_expired, now_epoch_secs, sort_locations_by_recency,
};
use crate::call::Location;
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transport::SipAddr;
use sea_orm::{ActiveModelTrait, Database, QueryOrder, Set, entity::prelude::*};
pub use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{
    big_integer, boolean, string_len, string_len_null, timestamp_with_time_zone as timestamp,
};
use sea_orm_migration::sea_query::ColumnDef as MigrationColumnDef;
use std::time::{Duration, Instant};
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
                    .col(
                        MigrationColumnDef::new(Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string_len(Column::Aor, 255).not_null())
                    .col(big_integer(Column::Expires).not_null())
                    .col(string_len(Column::Username, 200).not_null())
                    .col(string_len_null(Column::Realm, 200))
                    .col(string_len(Column::Destination, 255).not_null())
                    .col(string_len(Column::Transport, 32).not_null())
                    .col(big_integer(Column::LastModified).not_null())
                    .col(
                        timestamp(Column::CreatedAt)
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        timestamp(Column::UpdatedAt)
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(boolean(Column::SupportsWebrtc).not_null().default(false))
                    .col(string_len_null(Column::UserAgent, 255))
                    .to_owned(),
            )
            .await?;

        if !manager
            .has_index("rustpbx_locations", "idx_locations_realm_username")
            .await?
        {
            manager
                .create_index(
                    Index::create()
                        .table(Entity)
                        .name("idx_locations_realm_username")
                        .col(Column::Realm)
                        .col(Column::Username)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
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
        Self::new_with_migrate(url, true).await
    }

    pub async fn new_with_migrate(url: String, migrate: bool) -> Result<Self> {
        let db = Database::connect(&url)
            .await
            .map_err(|e| anyhow::anyhow!("Database connection error: {}", e))?;
        let db_locator = Self {
            db,
            realm_checker: Mutex::new(None),
        };
        if migrate {
            info!("Creating DbLocator with migration");
            match db_locator.migrate().await {
                Ok(_) => Ok(db_locator),
                Err(e) => {
                    warn!("migrate locator fail {}", e);
                    Err(e)
                }
            }
        } else {
            info!("Creating DbLocator without migration");
            Ok(db_locator)
        }
    }

    pub async fn migrate(&self) -> Result<()> {
        let manager = SchemaManager::new(&self.db);
        Migration
            .up(&manager)
            .await
            .map_err(|e| anyhow::anyhow!("Migration error: {}", e))?;
        Ok(())
    }
}

fn parse_transport_token(value: &str) -> Option<rsipstack::sip::transport::Transport> {
    super::routing::resolve_transport_from_str(value.trim())
}

fn encode_sip_addr(addr: &SipAddr) -> String {
    addr.to_string()
}

const HOME_PROXY_MARKER: &str = "|hp=";
const REGISTERED_AOR_MARKER: &str = "|ra=";

fn decode_sip_addr(value: &str) -> Option<SipAddr> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some((transport_raw, addr_raw)) = trimmed.split_once(' ')
        && let Some(transport) = parse_transport_token(transport_raw)
        && let Ok(addr) = rsipstack::sip::HostWithPort::try_from(addr_raw.trim())
    {
        return Some(SipAddr {
            r#type: Some(transport),
            addr,
        });
    }

    if let Ok(uri) = rsipstack::sip::Uri::try_from(trimmed)
        && let Ok(addr) = SipAddr::try_from(uri)
    {
        return Some(addr);
    }

    rsipstack::sip::HostWithPort::try_from(trimmed)
        .ok()
        .map(SipAddr::from)
}

fn encode_location_metadata(
    user_agent: Option<&str>,
    home_proxy: Option<&SipAddr>,
    registered_aor: Option<&rsipstack::sip::Uri>,
) -> Option<String> {
    let mut value = user_agent.unwrap_or("").to_string();

    if let Some(home_proxy) = home_proxy {
        value.push_str(HOME_PROXY_MARKER);
        value.push_str(encode_sip_addr(home_proxy).as_str());
    }

    if let Some(registered_aor) = registered_aor {
        value.push_str(REGISTERED_AOR_MARKER);
        value.push_str(registered_aor.to_string().as_str());
    }

    if value.is_empty() { None } else { Some(value) }
}

fn decode_location_metadata(
    value: Option<&str>,
) -> (Option<String>, Option<SipAddr>, Option<rsipstack::sip::Uri>) {
    let Some(raw) = value else {
        return (None, None, None);
    };

    let mut rest = raw;
    let mut home_proxy: Option<SipAddr> = None;
    let mut registered_aor: Option<rsipstack::sip::Uri> = None;

    loop {
        let Some((prefix, tail)) = rest.rsplit_once('|') else {
            break;
        };

        if home_proxy.is_none()
            && let Some(value) = tail.strip_prefix("hp=")
            && let Some(parsed) = decode_sip_addr(value)
        {
            home_proxy = Some(parsed);
            rest = prefix;
            continue;
        }

        if registered_aor.is_none()
            && let Some(value) = tail.strip_prefix("ra=")
            && let Ok(parsed) = rsipstack::sip::Uri::try_from(value)
        {
            registered_aor = Some(parsed);
            rest = prefix;
            continue;
        }

        break;
    }

    let user_agent = if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    };

    (user_agent, home_proxy, registered_aor)
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
        // Persist the actual network transport used by the destination address.
        // For WebSocket clients this preserves WSS even if Contact uses transport=ws.
        let SipAddr { r#type, addr } = destination;
        let transport = r#type
            .or(location.transport)
            .unwrap_or(rsipstack::sip::transport::Transport::Udp);
        let host = addr.to_string();

        let now = now_epoch_secs();

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
                active_model.user_agent = Set(encode_location_metadata(
                    location.user_agent.as_deref(),
                    location.home_proxy.as_ref(),
                    location.registered_aor.as_ref(),
                ));

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
                active_model.user_agent = Set(encode_location_metadata(
                    location.user_agent.as_deref(),
                    location.home_proxy.as_ref(),
                    location.registered_aor.as_ref(),
                ));

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

        // Only remove rows registered more than GRACE_SECS ago. This guards
        // against the rapid-reconnect race: a WS client reconnects and reuses
        // the same NAT ip:port, then the stale close event for the previous
        // connection arrives and would otherwise delete the fresh row.
        let now_epoch = now_epoch_secs();
        let cutoff = now_epoch - UNREGISTER_GRACE_SECS;

        let removed_locations = Entity::find()
            .filter(Column::Destination.eq(&host))
            .filter(Column::Transport.eq(&transport))
            .filter(Column::LastModified.lt(cutoff))
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on lookup before unregister: {}", e))?;
        if removed_locations.is_empty() {
            return Ok(None);
        }

        Entity::delete_many()
            .filter(Column::Destination.eq(&host))
            .filter(Column::Transport.eq(&transport))
            .filter(Column::LastModified.lt(cutoff))
            .exec(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on unregister with address: {}", e))?;

        let mut locations = Vec::new();
        for loc in removed_locations {
            let aor = rsipstack::sip::Uri::try_from(loc.aor.as_str())
                .map_err(|e| anyhow::anyhow!("Error parsing aor: {}", e))?;
            // Parse transport from string
            let transport = match loc.transport.to_uppercase().as_str() {
                "UDP" => rsipstack::sip::transport::Transport::Udp,
                "TCP" => rsipstack::sip::transport::Transport::Tcp,
                "TLS" => rsipstack::sip::transport::Transport::Tls,
                "WS" => rsipstack::sip::transport::Transport::Ws,
                "WSS" => rsipstack::sip::transport::Transport::Wss,
                _ => rsipstack::sip::transport::Transport::Udp, // Default to UDP
            };

            // Parse destination host to HostWithPort
            let addr = loc.destination.try_into()?;

            // Create SipAddr
            let destination = SipAddr {
                r#type: Some(transport),
                addr,
            };

            let (user_agent, home_proxy, decoded_registered_aor) =
                decode_location_metadata(loc.user_agent.as_deref());
            let registered_aor = choose_registered_aor(
                loc.username.as_str(),
                loc.realm.as_str(),
                &aor,
                decoded_registered_aor,
            );

            locations.push(Location {
                aor,
                expires: loc.expires as u32,
                destination: Some(destination),
                supports_webrtc: loc.supports_webrtc,
                transport: Some(transport),
                registered_aor: Some(registered_aor),
                user_agent,
                home_proxy,
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

    async fn lookup(&self, uri: &rsipstack::sip::Uri) -> Result<Vec<Location>> {
        let now_epoch = now_epoch_secs();

        let target_aor = uri.to_string();
        let mut models = Entity::find()
            .filter(Column::Aor.eq(&target_aor))
            .order_by_desc(Column::LastModified)
            .order_by_desc(Column::UpdatedAt)
            .order_by_desc(Column::Id)
            .all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on lookup by aor: {}", e))?;

        if models.is_empty() {
            if let Some(fb) = invalid_host_fallback(uri) {
                // 1. AoR-pattern match — handles WebRTC (JsSIP) clients where
                //    the Contact user part (e.g. "qn27nogk") differs from the
                //    authenticated username stored in Column::Username (e.g.
                //    "alice"), and extra params like ";ob" that break exact
                //    AoR match.
                models = Entity::find()
                    .filter(Column::Aor.like(fb.aor_like_pattern()))
                    .order_by_desc(Column::LastModified)
                    .order_by_desc(Column::UpdatedAt)
                    .order_by_desc(Column::Id)
                    .all(&self.db)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Database error on lookup by aor pattern for .invalid host: {}",
                            e
                        )
                    })?;

                // 2. Username fallback — original behaviour, useful when the
                //    Contact user part equals the auth username.
                if models.is_empty() {
                    models = Entity::find()
                        .filter(Column::Username.eq(&fb.user))
                        .order_by_desc(Column::LastModified)
                        .order_by_desc(Column::UpdatedAt)
                        .order_by_desc(Column::Id)
                        .all(&self.db)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "Database error on lookup by username for .invalid host: {}",
                                e
                            )
                        })?;
                }
            }
        }

        if models.is_empty() {
            let realm_raw = uri.host().to_string();
            let mut realm_key = realm_raw.trim().to_ascii_lowercase();
            if self.is_local_realm(&realm_key).await {
                realm_key = "localhost".to_string();
            }
            let username_raw = uri.user().unwrap_or("");
            let username_trimmed = username_raw.trim();
            let username_key = username_trimmed.to_ascii_lowercase();

            if !username_key.is_empty() {
                models = Entity::find()
                    .filter(Column::Username.eq(&username_key))
                    .filter(Column::Realm.eq(&realm_key))
                    .order_by_desc(Column::LastModified)
                    .order_by_desc(Column::UpdatedAt)
                    .order_by_desc(Column::Id)
                    .all(&self.db)
                    .await
                    .map_err(|e| anyhow::anyhow!("Database error on lookup: {}", e))?;

                if models.is_empty()
                    && (realm_key.is_empty() || self.is_local_realm(&realm_key).await)
                {
                    models = Entity::find()
                        .filter(Column::Username.eq(&username_key))
                        .order_by_desc(Column::LastModified)
                        .order_by_desc(Column::UpdatedAt)
                        .order_by_desc(Column::Id)
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
        let mut expired_ids = Vec::new();
        let now_instant = Instant::now();
        for model in models {
            if is_location_expired(model.expires, model.last_modified, now_epoch) {
                expired_ids.push(model.id);
                continue;
            }
            let aor = rsipstack::sip::Uri::try_from(model.aor.as_str())
                .map_err(|e| anyhow::anyhow!("Error parsing aor: {}", e))?;

            let (user_agent, home_proxy, decoded_registered_aor) =
                decode_location_metadata(model.user_agent.as_deref());
            let registered_aor = choose_registered_aor(
                model.username.as_str(),
                model.realm.as_str(),
                &aor,
                decoded_registered_aor,
            );

            // Parse transport from string
            let transport = match model.transport.to_uppercase().as_str() {
                "UDP" => rsipstack::sip::transport::Transport::Udp,
                "TCP" => rsipstack::sip::transport::Transport::Tcp,
                "TLS" => rsipstack::sip::transport::Transport::Tls,
                "WS" => rsipstack::sip::transport::Transport::Ws,
                "WSS" => rsipstack::sip::transport::Transport::Wss,
                _ => rsipstack::sip::transport::Transport::Udp, // Default to UDP
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
                user_agent,
                home_proxy,
                ..Default::default()
            });
        }

        // Best-effort cleanup of expired bindings so they don't shadow live
        // registrations in subsequent .invalid username lookups (which order by
        // recency). Expired rows were previously only skipped, never deleted.
        if !expired_ids.is_empty()
            && let Err(e) = Entity::delete_many()
                .filter(Column::Id.is_in(expired_ids))
                .exec(&self.db)
                .await
        {
            warn!(error = %e, "Failed to delete expired location rows during lookup");
        }

        Ok(sort_locations_by_recency(locations))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::transport::Transport;
    use rsipstack::transport::SipAddr;

    #[tokio::test]
    async fn register_prefers_destination_transport_for_websocket_connections() {
        let locator = DbLocator::new_with_migrate("sqlite::memory:".to_string(), true)
            .await
            .expect("create db locator");

        let aor: rsipstack::sip::Uri = "sip:hvd34mgb@tvuug6sjfcbi.invalid;transport=ws"
            .try_into()
            .expect("valid aor");

        let destination = SipAddr {
            r#type: Some(Transport::Wss),
            addr: "122.235.198.105:24534"
                .try_into()
                .expect("valid destination"),
        };

        let location = Location {
            aor: aor.clone(),
            expires: 600,
            destination: Some(destination),
            // Contact transport remains ws, but destination is WSS.
            transport: Some(Transport::Ws),
            ..Default::default()
        };

        locator
            .register("bob", Some("kefutest.xiaojukeji.com"), location)
            .await
            .expect("register location");

        let locations = locator.lookup(&aor).await.expect("lookup location");
        assert_eq!(locations.len(), 1);

        let stored = &locations[0];
        assert_eq!(stored.transport, Some(Transport::Wss));
        assert_eq!(
            stored.destination.as_ref().and_then(|d| d.r#type),
            Some(Transport::Wss)
        );
    }

    /// Helper: create an in-memory DbLocator and register a WebRTC binding.
    async fn make_locator_with_webrtc_binding(
        auth_username: &str,
        contact_uri: &str,
        dest_addr: &str,
    ) -> DbLocator {
        let locator = DbLocator::new_with_migrate("sqlite::memory:".to_string(), true)
            .await
            .expect("create db locator");

        let aor: rsipstack::sip::Uri = contact_uri.try_into().expect("valid aor");
        let destination = SipAddr {
            r#type: Some(Transport::Wss),
            addr: dest_addr.try_into().expect("valid destination"),
        };
        let location = Location {
            aor: aor.clone(),
            expires: 600,
            destination: Some(destination),
            transport: Some(Transport::Ws),
            ..Default::default()
        };
        locator
            .register(auth_username, Some("pbx.example.com"), location)
            .await
            .expect("register location");
        locator
    }

    /// Regression: JsSIP generates a random Contact user part (e.g. "qn27nogk")
    /// that differs from the authenticated username (e.g. "xwork_5g_test").
    /// A BYE's Request-URI uses the Contact from the INVITE, so the lookup must
    /// resolve the random user part back to the stored registration.
    #[tokio::test]
    async fn invalid_host_lookup_finds_binding_when_contact_user_differs_from_auth_username() {
        let locator = make_locator_with_webrtc_binding(
            "xwork_5g_test",
            "sip:qn27nogk@2kbkhn3beiif.invalid;transport=ws",
            "58.40.134.154:41428",
        )
        .await;

        // Lookup with the same Contact as the INVITE (has ";ob" — exact AoR miss,
        // and user part "qn27nogk" ≠ stored username "xwork_5g_test").
        let lookup_uri: rsipstack::sip::Uri =
            "sip:qn27nogk@2kbkhn3beiif.invalid;transport=ws;ob"
                .try_into()
                .expect("valid lookup uri");

        let locations = locator
            .lookup(&lookup_uri)
            .await
            .expect("lookup location");

        assert_eq!(
            locations.len(),
            1,
            "AoR pattern match should find the WebRTC binding despite \
             user-part / username mismatch and extra ;ob param"
        );

        let dest = locations[0].destination.as_ref().expect("has destination");
        assert_eq!(dest.addr.to_string(), "58.40.134.154:41428");
    }

    /// Regression: the BYE Request-URI may carry extra params (;ob) that the
    /// stored AoR does not have.  The AoR-pattern match must bridge this gap.
    #[tokio::test]
    async fn invalid_host_lookup_with_ob_param_finds_exact_aor_without_ob() {
        let locator = make_locator_with_webrtc_binding(
            "alice",
            "sip:abc123@xyz.invalid;transport=ws",
            "10.0.0.1:5060",
        )
        .await;

        let lookup_uri: rsipstack::sip::Uri =
            "sip:abc123@xyz.invalid;transport=ws;ob"
                .try_into()
                .expect("valid lookup uri");

        let locations = locator
            .lookup(&lookup_uri)
            .await
            .expect("lookup location");

        assert_eq!(locations.len(), 1, ";ob param should not break AoR pattern match");
    }

    /// The original Username-based fallback must still work for .invalid hosts
    /// where the Contact user part equals the auth username.
    #[tokio::test]
    async fn invalid_host_lookup_username_fallback_still_works() {
        let locator = make_locator_with_webrtc_binding(
            "agent42",
            "sip:agent42@webrtc.invalid;transport=ws",
            "192.168.1.100:443",
        )
        .await;

        // Lookup with a *different* contact user part that does NOT match any AoR
        // pattern, but shares the same .invalid host. Username fallback should kick in.
        // (In practice this is rare — both paths exist for defence-in-depth.)
        let lookup_uri: rsipstack::sip::Uri =
            "sip:agent42@different.invalid;transport=ws"
                .try_into()
                .expect("valid lookup uri");

        let locations = locator
            .lookup(&lookup_uri)
            .await
            .expect("lookup location");

        // The host is different so AoR LIKE won't match, but the general
        // username/realm fallback (line 558+) should find it.
        assert_eq!(locations.len(), 1, "username/realm fallback should still work");
    }

    /// Negative test: a completely unrelated .invalid URI must return empty.
    #[tokio::test]
    async fn invalid_host_lookup_returns_empty_for_unknown_contact() {
        let locator = make_locator_with_webrtc_binding(
            "alice",
            "sip:abc123@xyz.invalid;transport=ws",
            "10.0.0.1:5060",
        )
        .await;

        let lookup_uri: rsipstack::sip::Uri =
            "sip:nobody@noreg.invalid;transport=ws"
                .try_into()
                .expect("valid lookup uri");

        let locations = locator
            .lookup(&lookup_uri)
            .await
            .expect("lookup location");

        assert!(
            locations.is_empty(),
            "unknown .invalid contact should return empty"
        );
    }
}
