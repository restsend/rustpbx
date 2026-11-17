//! Push-capable device registrations.
//!
//! Locator bindings disappear when a device's flow closes, which is exactly
//! when push is needed, so push devices are tracked separately: in memory for
//! routing, and in the `rustpbx_push_devices` table (when a database is
//! available) so a PBX restart does not strand every app that is asleep.

use super::PushParams;
use crate::call::Location;
use parking_lot::RwLock;
use rsipstack::sip::Uri;
use rsipstack::sip::uri::ParamsExt;
use rsipstack::transport::SipAddr;
use rustpbx_models::push_device;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Instant;
use tracing::debug;

pub use crate::proxy::locator::now_epoch_secs;

/// A device that asked to be woken by push notifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushDevice {
    /// Locator identifier of the registered user (`username@realm`).
    pub identity: String,
    pub username: String,
    pub realm: Option<String>,
    /// Registered address-of-record (REGISTER To URI).
    pub aor: String,
    /// Last registered Contact URI.
    pub contact: String,
    /// RFC 5626 `+sip.instance`, when the UA sends one.
    pub instance_id: Option<String>,
    pub params: PushParams,
    pub user_agent: Option<String>,
    /// Unix seconds when the binding expires unless refreshed.
    pub expires_at: i64,
    /// Unix seconds of the REGISTER that created/refreshed the binding.
    pub registered_at: i64,
    /// Unix seconds of the binding-refresh push sent for this registration.
    pub refresh_pushed_at: Option<i64>,
    /// Flow of the device's binding (the locator binding's destination). Not
    /// persisted: after a restart it is derived from the Contact.
    pub destination: Option<SipAddr>,
}

impl PushDevice {
    /// Stable key of the device within its identity: the `+sip.instance` when
    /// the UA sends one, else a digest of its push resource ID.
    pub fn device_key(&self) -> String {
        match self.instance_id.as_deref() {
            Some(instance) if instance.len() <= 200 => format!("instance:{}", instance),
            Some(instance) => format!("instance:{}", digest(instance)),
            None => format!(
                "prid:{}",
                digest(&format!("{}:{}", self.params.provider, self.params.prid))
            ),
        }
    }

    /// Whether a binding with `instance_id` / `params` belongs to this device:
    /// same RFC 5626 instance, or same push resource ID (RFC 8599 §5.3 matches
    /// bindings on their `pn-*` parameters).
    pub fn same_device(&self, instance_id: Option<&str>, params: Option<&PushParams>) -> bool {
        if let (Some(mine), Some(theirs)) = (self.instance_id.as_deref(), instance_id)
            && mine == theirs
        {
            return true;
        }
        params.is_some_and(|p| p.provider == self.params.provider && p.prid == self.params.prid)
    }

    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at <= now
    }

    /// The locator binding of this device, as the registrar stored it.
    pub fn binding(&self, now: i64) -> Option<Location> {
        let contact = Uri::try_from(self.contact.as_str()).ok()?;
        Some(Location {
            transport: contact.transport().copied(),
            destination: self
                .destination
                .clone()
                .or_else(|| SipAddr::try_from(&contact).ok()),
            aor: contact,
            expires: (self.expires_at - now).max(0) as u32,
            last_modified: Some(Instant::now()),
            registered_username: Some(self.username.clone()),
            registered_realm: self.realm.clone(),
            registered_aor: Uri::try_from(self.aor.as_str()).ok(),
            contact_raw: Some(self.contact.clone()),
            instance_id: self.instance_id.clone(),
            user_agent: self.user_agent.clone(),
            ..Default::default()
        })
    }

    fn from_model(model: push_device::Model) -> Self {
        let destination = Uri::try_from(model.contact.as_str())
            .ok()
            .and_then(|contact| SipAddr::try_from(&contact).ok());
        Self {
            destination,
            identity: model.identity,
            username: model.username,
            realm: model.realm,
            aor: model.aor,
            contact: model.contact,
            instance_id: model.instance_id,
            params: PushParams {
                provider: model.provider,
                prid: model.prid,
                param: model.param,
            },
            user_agent: model.user_agent,
            expires_at: model.expires_at,
            registered_at: model.registered_at,
            refresh_pushed_at: model.refresh_pushed_at,
        }
    }

    fn to_active_model(&self) -> push_device::ActiveModel {
        push_device::ActiveModel {
            identity: Set(self.identity.clone()),
            device_key: Set(self.device_key()),
            username: Set(self.username.clone()),
            realm: Set(self.realm.clone()),
            aor: Set(self.aor.clone()),
            contact: Set(self.contact.clone()),
            instance_id: Set(self.instance_id.clone()),
            provider: Set(self.params.provider.clone()),
            prid: Set(self.params.prid.clone()),
            param: Set(self.params.param.clone()),
            user_agent: Set(self.user_agent.clone()),
            expires_at: Set(self.expires_at),
            registered_at: Set(self.registered_at),
            refresh_pushed_at: Set(self.refresh_pushed_at),
        }
    }
}

fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Push devices by identity, optionally persisted to the database.
pub struct PushRegistry {
    devices: RwLock<HashMap<String, Vec<PushDevice>>>,
    db: Option<DatabaseConnection>,
}

impl Default for PushRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PushRegistry {
    /// In-memory registry (devices are lost on restart).
    pub fn new() -> Self {
        Self {
            devices: RwLock::new(HashMap::new()),
            db: None,
        }
    }

    /// Registry persisted to `db`, loaded with the devices still registered.
    pub async fn with_database(db: DatabaseConnection) -> anyhow::Result<Self> {
        let now = now_epoch_secs();
        let rows = push_device::Entity::find()
            .filter(push_device::Column::ExpiresAt.gt(now))
            .all(&db)
            .await
            .map_err(|e| anyhow::anyhow!("failed to load push devices: {}", e))?;
        let mut devices: HashMap<String, Vec<PushDevice>> = HashMap::new();
        for row in rows {
            let device = PushDevice::from_model(row);
            devices
                .entry(device.identity.clone())
                .or_default()
                .push(device);
        }
        Ok(Self {
            devices: RwLock::new(devices),
            db: Some(db),
        })
    }

    pub fn len(&self) -> usize {
        self.devices.read().values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All unexpired devices.
    pub fn all(&self, now: i64) -> Vec<PushDevice> {
        self.devices
            .read()
            .values()
            .flatten()
            .filter(|device| !device.is_expired(now))
            .cloned()
            .collect()
    }

    /// The device registered with this push resource ID.
    pub fn device_with_prid(&self, params: &PushParams) -> Option<PushDevice> {
        self.devices
            .read()
            .values()
            .flatten()
            .find(|d| d.params.provider == params.provider && d.params.prid == params.prid)
            .cloned()
    }

    /// The device of `username` whose binding flow is `destination`.
    pub fn device_at(&self, destination: &SipAddr, username: &str) -> Option<PushDevice> {
        self.devices
            .read()
            .values()
            .flatten()
            .find(|d| {
                d.destination.as_ref() == Some(destination)
                    && d.username.eq_ignore_ascii_case(username)
            })
            .cloned()
    }

    /// Unexpired devices registered for `identity`.
    pub fn devices(&self, identity: &str, now: i64) -> Vec<PushDevice> {
        self.devices
            .read()
            .get(identity)
            .map(|list| {
                list.iter()
                    .filter(|device| !device.is_expired(now))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record a (re-)registration. Any other entry of the same identity for
    /// the same device (e.g. stored before its push token rotated) is
    /// replaced.
    pub async fn upsert(&self, device: PushDevice) {
        let key = device.device_key();
        let replaced: Vec<PushDevice> = {
            let mut map = self.devices.write();
            let list = map.entry(device.identity.clone()).or_default();
            let mut replaced = Vec::new();
            list.retain(|existing| {
                let same = existing.device_key() == key
                    || existing.same_device(device.instance_id.as_deref(), Some(&device.params));
                if same && existing.device_key() != key {
                    replaced.push(existing.clone());
                }
                !same
            });
            list.push(device.clone());
            replaced
        };
        debug!(identity = %device.identity, device = %key, expires_at = device.expires_at, "push device registered");
        if let Some(db) = &self.db {
            for old in &replaced {
                delete_row(db, &old.identity, &old.device_key()).await;
            }
            persist(db, &device).await;
        }
    }

    /// Forget the devices of `identity` matching `instance_id` / `params`
    /// (an unregistration or a REGISTER that no longer requests push).
    pub async fn remove_device(
        &self,
        identity: &str,
        instance_id: Option<&str>,
        params: Option<&PushParams>,
    ) -> Vec<PushDevice> {
        if instance_id.is_none() && params.is_none() {
            return Vec::new();
        }
        let removed = {
            let mut map = self.devices.write();
            let Some(list) = map.get_mut(identity) else {
                return Vec::new();
            };
            let mut removed = Vec::new();
            list.retain(|device| {
                let same = device.same_device(instance_id, params);
                if same {
                    removed.push(device.clone());
                }
                !same
            });
            if list.is_empty() {
                map.remove(identity);
            }
            removed
        };
        self.delete_rows(&removed).await;
        removed
    }

    /// Forget every device of `identity` (wildcard unregistration).
    pub async fn remove_identity(&self, identity: &str) -> Vec<PushDevice> {
        let removed = self.devices.write().remove(identity).unwrap_or_default();
        self.delete_rows(&removed).await;
        removed
    }

    /// Forget every device registered with this push resource ID.
    pub async fn remove_prid(&self, params: &PushParams) -> Vec<PushDevice> {
        let removed = self.remove_where(|device| {
            device.params.provider == params.provider && device.params.prid == params.prid
        });
        self.delete_rows(&removed).await;
        removed
    }

    /// Devices that should be pushed now so they refresh their binding before
    /// it expires (RFC 8599 §5.5). Bindings too short-lived for this to be
    /// useful are left to the UA.
    pub fn due_for_refresh(&self, now: i64, refresh_before: i64) -> Vec<PushDevice> {
        self.devices
            .read()
            .values()
            .flatten()
            .filter(|device| {
                device.refresh_pushed_at.is_none()
                    && !device.is_expired(now)
                    && device.expires_at - device.registered_at > 2 * refresh_before
                    && now >= device.expires_at - refresh_before
            })
            .cloned()
            .collect()
    }

    pub async fn mark_refresh_pushed(&self, device: &PushDevice, at: i64) {
        let key = device.device_key();
        let updated = {
            let mut map = self.devices.write();
            map.get_mut(&device.identity)
                .and_then(|list| list.iter_mut().find(|d| d.device_key() == key))
                .map(|stored| {
                    stored.refresh_pushed_at = Some(at);
                    stored.clone()
                })
        };
        if let (Some(db), Some(updated)) = (&self.db, updated) {
            persist(db, &updated).await;
        }
    }

    /// Drop devices whose binding expired.
    pub async fn sweep_expired(&self, now: i64) -> Vec<PushDevice> {
        let removed = self.remove_where(|device| device.is_expired(now));
        if let Some(db) = &self.db
            && let Err(e) = push_device::Entity::delete_many()
                .filter(push_device::Column::ExpiresAt.lte(now))
                .exec(db)
                .await
        {
            crate::db_report::report_db_write_failure("push_devices", "sweep", None, e);
        }
        removed
    }

    fn remove_where(&self, mut predicate: impl FnMut(&PushDevice) -> bool) -> Vec<PushDevice> {
        let mut removed = Vec::new();
        self.devices.write().retain(|_, list| {
            list.retain(|device| {
                if predicate(device) {
                    removed.push(device.clone());
                    false
                } else {
                    true
                }
            });
            !list.is_empty()
        });
        removed
    }

    async fn delete_rows(&self, devices: &[PushDevice]) {
        if let Some(db) = &self.db {
            for device in devices {
                delete_row(db, &device.identity, &device.device_key()).await;
            }
        }
    }
}

async fn persist(db: &DatabaseConnection, device: &PushDevice) {
    use push_device::Column;
    let result = push_device::Entity::insert(device.to_active_model())
        .on_conflict(
            OnConflict::columns([Column::Identity, Column::DeviceKey])
                .update_columns([
                    Column::Username,
                    Column::Realm,
                    Column::Aor,
                    Column::Contact,
                    Column::InstanceId,
                    Column::Provider,
                    Column::Prid,
                    Column::Param,
                    Column::UserAgent,
                    Column::ExpiresAt,
                    Column::RegisteredAt,
                    Column::RefreshPushedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await;
    if let Err(e) = result {
        crate::db_report::report_db_write_failure("push_devices", "upsert", None, e);
    }
}

async fn delete_row(db: &DatabaseConnection, identity: &str, device_key: &str) {
    let result = push_device::Entity::delete_many()
        .filter(push_device::Column::Identity.eq(identity))
        .filter(push_device::Column::DeviceKey.eq(device_key))
        .exec(db)
        .await;
    if let Err(e) = result {
        crate::db_report::report_db_write_failure("push_devices", "delete", None, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm_migration::MigratorTrait;

    fn device(identity: &str, instance: Option<&str>, prid: &str, expires_at: i64) -> PushDevice {
        PushDevice {
            identity: identity.to_string(),
            username: identity.split('@').next().unwrap().to_string(),
            realm: Some("localhost".to_string()),
            aor: format!("sip:{}", identity),
            contact: format!(
                "sip:{}:5061;transport=tls;pn-provider=fcm;pn-prid={}",
                identity, prid
            ),
            instance_id: instance.map(str::to_string),
            params: PushParams {
                provider: "fcm".to_string(),
                prid: prid.to_string(),
                param: Some("123".to_string()),
            },
            user_agent: None,
            expires_at,
            registered_at: expires_at - 3600,
            refresh_pushed_at: None,
            destination: None,
        }
    }

    #[tokio::test]
    async fn reregistration_replaces_same_device() {
        let registry = PushRegistry::new();
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:1>"),
                "token-a",
                5000,
            ))
            .await;
        // Same instance, rotated token.
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:1>"),
                "token-b",
                6000,
            ))
            .await;
        // Same token, instance id appeared: still the same app install.
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:2>"),
                "token-b",
                7000,
            ))
            .await;
        let devices = registry.devices("bob@localhost", 1000);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].params.prid, "token-b");
        assert_eq!(devices[0].expires_at, 7000);

        // Another device of the same user is kept apart.
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:3>"),
                "token-c",
                7000,
            ))
            .await;
        assert_eq!(registry.devices("bob@localhost", 1000).len(), 2);
    }

    #[tokio::test]
    async fn removal_and_expiry() {
        let registry = PushRegistry::new();
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:1>"),
                "token-a",
                5000,
            ))
            .await;
        registry
            .upsert(device("alice@localhost", None, "token-x", 2000))
            .await;
        assert_eq!(
            registry.devices("alice@localhost", 2000).len(),
            0,
            "expired"
        );

        let removed = registry
            .remove_device("bob@localhost", Some("<urn:uuid:1>"), None)
            .await;
        assert_eq!(removed.len(), 1);
        assert!(registry.devices("bob@localhost", 1000).is_empty());

        assert_eq!(registry.sweep_expired(2000).await.len(), 1);
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn refresh_scheduling() {
        let registry = PushRegistry::new();
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:1>"),
                "token-a",
                10_000,
            ))
            .await;
        // registered_at = 6400, lifetime 3600 > 2 * 300.
        assert!(registry.due_for_refresh(9_000, 300).is_empty());
        let due = registry.due_for_refresh(9_700, 300);
        assert_eq!(due.len(), 1);
        registry.mark_refresh_pushed(&due[0], 9_700).await;
        assert!(
            registry.due_for_refresh(9_800, 300).is_empty(),
            "pushed once per registration"
        );

        // Too short-lived to be worth refreshing.
        let mut short = device("carol@localhost", None, "token-c", 10_000);
        short.registered_at = 9_500;
        registry.upsert(short).await;
        assert!(registry.due_for_refresh(9_800, 300).is_empty());
    }

    #[tokio::test]
    async fn persists_across_restarts() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        crate::models::migration::Migrator::up(&db, None)
            .await
            .unwrap();
        let now = now_epoch_secs();

        let registry = PushRegistry::with_database(db.clone()).await.unwrap();
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:1>"),
                "token-a",
                now + 3600,
            ))
            .await;
        registry
            .upsert(device(
                "bob@localhost",
                Some("<urn:uuid:1>"),
                "token-b",
                now + 7200,
            ))
            .await;
        registry
            .upsert(device("dave@localhost", None, "token-d", now + 3600))
            .await;
        registry
            .remove_prid(&PushParams {
                provider: "fcm".into(),
                prid: "token-d".into(),
                param: None,
            })
            .await;
        let pushed = registry.devices("bob@localhost", now)[0].clone();
        registry.mark_refresh_pushed(&pushed, now).await;

        let reloaded = PushRegistry::with_database(db).await.unwrap();
        assert_eq!(reloaded.len(), 1);
        let devices = reloaded.devices("bob@localhost", now);
        assert_eq!(devices[0].params.prid, "token-b");
        assert_eq!(devices[0].refresh_pushed_at, Some(now));
    }
}
