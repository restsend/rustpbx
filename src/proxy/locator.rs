use super::locator_db::DbLocator;
use crate::{
    call::Location,
    config::{LocatorConfig, ProxyConfig},
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::{
    transaction::endpoint::{TargetLocator, TransportEventInspector},
    transport::{SipAddr, TransportEvent},
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Instant,
};
use tokio::sync::Mutex;
use tracing::debug;

#[derive(Clone, Debug)]
pub enum LocatorEvent {
    Registered(Location),
    Unregistered(Location),
    Offline(Vec<Location>),
}

pub type LocatorEventSender = tokio::sync::broadcast::Sender<LocatorEvent>;
pub type LocatorEventReceiver = tokio::sync::broadcast::Receiver<LocatorEvent>;
pub type LocatorCreationFuture = Pin<Box<dyn Future<Output = Result<Box<dyn Locator>>> + Send>>;
pub type RealmChecker =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[async_trait]
pub trait Locator: Send + Sync {
    async fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        let username = user.trim().to_ascii_lowercase();
        let realm = match realm {
            Some(r) if !r.trim().is_empty() => {
                let r = r.trim();
                if self.is_local_realm(r).await {
                    Some("localhost".to_string())
                } else {
                    Some(r.to_ascii_lowercase())
                }
            }
            _ => None,
        };

        match (username.is_empty(), realm) {
            (true, _) => String::new(),
            (false, Some(realm)) => format!("{}@{}", username, realm),
            (false, None) => username,
        }
    }
    async fn is_local_realm(&self, realm: &str) -> bool {
        is_local_realm(realm)
    }
    fn set_realm_checker(&self, _checker: RealmChecker) {}
    async fn register(&self, username: &str, realm: Option<&str>, location: Location)
    -> Result<()>;
    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()>;
    async fn unregister_with_address(&self, addr: &SipAddr) -> Result<Option<Vec<Location>>>;
    async fn lookup(&self, uri: &rsip::Uri) -> Result<Vec<Location>>;
}

pub struct DialogTargetLocator {
    locator: Arc<Box<dyn Locator>>,
}

impl DialogTargetLocator {
    pub fn new(locator: Arc<Box<dyn Locator>>) -> Box<dyn TargetLocator> {
        Box::new(Self { locator }) as Box<dyn TargetLocator>
    }
}

#[async_trait]
impl TargetLocator for DialogTargetLocator {
    async fn locate(&self, uri: &rsip::Uri) -> Result<SipAddr, rsipstack::Error> {
        match self.locator.lookup(uri).await {
            Ok(locs) => {
                if let Some(loc) = locs.first() {
                    if let Some(dest) = &loc.destination {
                        debug!(%uri, %dest, "Located target for dialog");
                        return Ok(dest.clone());
                    }
                }
            }
            Err(_) => {}
        }
        SipAddr::try_from(uri).map_err(|e| {
            rsipstack::Error::Error(format!(
                "failed to convert uri to sip addr: {}, error: {}",
                uri, e
            ))
        })
    }
}

pub struct TransportInspectorLocator {
    locator_events: LocatorEventSender,
    locator: Arc<Box<dyn Locator>>,
}

impl TransportInspectorLocator {
    pub fn new(
        locator: Arc<Box<dyn Locator>>,
        locator_events: LocatorEventSender,
    ) -> Box<dyn TransportEventInspector> {
        Box::new(Self {
            locator,
            locator_events,
        }) as Box<dyn TransportEventInspector>
    }
}

#[async_trait]
impl TransportEventInspector for TransportInspectorLocator {
    async fn handle(&self, event: TransportEvent) -> Option<TransportEvent> {
        match &event {
            TransportEvent::Closed(conn) => {
                match self.locator.unregister_with_address(conn.get_addr()).await {
                    Ok(Some(removed)) => {
                        if !removed.is_empty() {
                            self.locator_events
                                .send(LocatorEvent::Offline(removed))
                                .ok();
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        debug!(error = %e, "Error unregistering location on transport close");
                    }
                }
            }
            _ => {}
        }
        Some(event)
    }
}

pub struct MemoryLocator {
    locations: Mutex<HashMap<String, HashMap<String, Location>>>,
    realm_checker: Mutex<Option<RealmChecker>>,
}

impl Default for MemoryLocator {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryLocator {
    pub fn new() -> Self {
        Self {
            locations: Mutex::new(HashMap::new()),
            realm_checker: Mutex::new(None),
        }
    }
    pub fn create(_config: Arc<ProxyConfig>) -> LocatorCreationFuture {
        Box::pin(async move { Ok(Box::new(MemoryLocator::new()) as Box<dyn Locator>) })
    }
}

#[async_trait]
impl Locator for MemoryLocator {
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
        let identifier = self.get_identifier(username, realm).await;
        if identifier.is_empty() {
            debug!("skip registering location with empty identifier");
            return Ok(());
        }
        let mut location = location;
        let key = location.binding_key();
        debug!(identifier, binding = %key, %location, "Registering");
        let mut locations = self.locations.lock().await;
        let entry = locations
            .entry(identifier.clone())
            .or_insert_with(HashMap::new);
        if location.expires == 0 {
            entry.remove(&key);
            if entry.is_empty() {
                locations.remove(&identifier);
            }
        } else {
            if location.last_modified.is_none() {
                location.last_modified = Some(Instant::now());
            }
            entry.insert(key, location);
        }
        Ok(())
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm).await;
        let mut locations = self.locations.lock().await;
        locations.remove(&identifier);
        Ok(())
    }

    async fn unregister_with_address(&self, addr: &SipAddr) -> Result<Option<Vec<Location>>> {
        let mut locations = self.locations.lock().await;
        let mut identifiers_to_remove = Vec::new();
        let mut removed_locations = Vec::new();

        for (identifier, map) in locations.iter_mut() {
            let keys_to_remove: Vec<String> = map
                .iter()
                .filter_map(|(key, loc)| {
                    if let Some(dest) = &loc.destination {
                        if dest == addr {
                            return Some(key.clone());
                        }
                    }
                    None
                })
                .collect();

            for key in keys_to_remove {
                if let Some(loc) = map.remove(&key) {
                    removed_locations.push(loc);
                }
            }

            if map.is_empty() {
                identifiers_to_remove.push(identifier.clone());
            }
        }

        for identifier in identifiers_to_remove {
            if let Some(locs) = locations.remove(&identifier) {
                for loc in locs.values() {
                    removed_locations.push(loc.clone());
                }
            }
        }

        if removed_locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(removed_locations))
        }
    }

    async fn lookup(&self, uri: &rsip::Uri) -> Result<Vec<Location>> {
        let mut locations = self.locations.lock().await;
        let now: Instant = Instant::now();
        let uri_string = uri.to_string();
        let mut direct_hits = Vec::new();

        // Prune expired bindings and attempt direct contact/GRUU matches first
        locations.retain(|_, map| {
            map.retain(|_, loc| !loc.is_expired_at(now));
            !map.is_empty()
        });
        for map in locations.values() {
            for loc in map.values() {
                if &loc.aor == uri || uri_matches(&loc.aor, uri) {
                    direct_hits.push(loc.clone());
                    continue;
                }
                if let Some(registered) = &loc.registered_aor {
                    if registered == uri || uri_matches(registered, uri) {
                        direct_hits.push(loc.clone());
                        continue;
                    }
                }
                if let Some(gruu) = &loc.gruu {
                    if gruu == &uri_string || gruu.eq_ignore_ascii_case(&uri_string) {
                        direct_hits.push(loc.clone());
                        continue;
                    }
                }
            }
        }

        if !direct_hits.is_empty() {
            return Ok(sort_locations_by_recency(direct_hits));
        }

        // Fall back to classic AoR lookup by username/realm
        let username_raw = uri.user().unwrap_or_else(|| "");
        let username = username_raw.trim();
        let username_lower = username.to_ascii_lowercase();
        let realm_raw = uri.host().to_string();
        let realm_trimmed = realm_raw.trim();

        let mut identifiers = Vec::new();
        if !username.is_empty() {
            if !realm_trimmed.is_empty() {
                identifiers.push(self.get_identifier(username, Some(realm_trimmed)).await);
            }
            identifiers.push(self.get_identifier(username, Some("localhost")).await);
            identifiers.push(self.get_identifier(username, None).await);
        }

        for id in identifiers {
            if let Some(map) = locations.get(&id) {
                if !map.is_empty() {
                    let results: Vec<_> = map.values().cloned().collect();
                    return Ok(sort_locations_by_recency(results));
                }
            }
        }

        if !username.is_empty() {
            let mut fallback_hits = Vec::new();

            for map in locations.values() {
                for loc in map.values() {
                    let mut matched = false;

                    if let Some(registered) = &loc.registered_aor {
                        let user_match = registered
                            .user()
                            .map(|u| u.trim().eq_ignore_ascii_case(&username_lower))
                            .unwrap_or(false);
                        let realm_string = registered.host().to_string();
                        let realm_match =
                            realm_matches(self, realm_trimmed, realm_string.trim()).await;

                        if user_match && realm_match {
                            matched = true;
                        }
                    }

                    if !matched {
                        let user_match = loc
                            .aor
                            .user()
                            .map(|u| u.trim().eq_ignore_ascii_case(&username_lower))
                            .unwrap_or(false);
                        let realm_string = loc.aor.host().to_string();
                        let realm_match =
                            realm_matches(self, realm_trimmed, realm_string.trim()).await;

                        if user_match && realm_match {
                            matched = true;
                        }
                    }

                    if matched {
                        fallback_hits.push(loc.clone());
                    }
                }
            }

            if !fallback_hits.is_empty() {
                return Ok(sort_locations_by_recency(fallback_hits));
            }
        }

        Ok(vec![])
    }
}

fn compare_location_recency(a: &Location, b: &Location) -> Ordering {
    match (a.last_modified, b.last_modified) {
        (Some(a_ts), Some(b_ts)) => b_ts.cmp(&a_ts),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn host_without_port(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.starts_with('[') {
        if let Some(end) = trimmed.find(']') {
            return &trimmed[1..end];
        }
    }

    // If it contains more than one colon, it's likely an IPv6 address without brackets
    if trimmed.matches(':').count() > 1 {
        return trimmed;
    }

    trimmed.split(':').next().unwrap_or(trimmed)
}

pub(crate) fn is_local_realm(realm: &str) -> bool {
    if realm.trim().is_empty() {
        return false;
    }
    let host = host_without_port(realm);
    let host_lower = host.to_ascii_lowercase();
    if matches!(
        host_lower.as_str(),
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1"
    ) {
        return true;
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            return true;
        }
        if let std::net::IpAddr::V4(v4) = ip {
            if v4.is_private() {
                return true;
            }
        }
    }
    false
}

async fn realm_matches(
    locator: &dyn Locator,
    requested_realm: &str,
    candidate_realm: &str,
) -> bool {
    let requested = requested_realm.trim();
    if requested.is_empty() {
        return true;
    }

    let candidate = candidate_realm.trim();
    if locator.is_local_realm(requested).await && locator.is_local_realm(candidate).await {
        return true;
    }

    let requested_host = host_without_port(requested).to_ascii_lowercase();
    let candidate_host = host_without_port(candidate).to_ascii_lowercase();
    requested_host == candidate_host
}

pub fn uri_matches(a: &rsip::Uri, b: &rsip::Uri) -> bool {
    if a == b {
        return true;
    }

    if a.user() == b.user() {
        let a_host = a.host().to_string();
        let b_host = b.host().to_string();

        if is_local_realm(&a_host) && is_local_realm(&b_host) {
            return true;
        }

        // Special handling for .invalid domains often used in WebRTC
        if a_host.ends_with(".invalid") || b_host.ends_with(".invalid") {
            if a_host.eq_ignore_ascii_case(&b_host) {
                return true;
            }
        }
    }

    // Fallback to case-insensitive string comparison for the whole URI
    // This handles transport=ws vs transport=WS
    a.to_string().eq_ignore_ascii_case(&b.to_string())
}

pub(crate) fn sort_locations_by_recency(mut locations: Vec<Location>) -> Vec<Location> {
    locations.sort_by(compare_location_recency);
    let mut seen: HashSet<String> = HashSet::new();
    locations.retain(|loc| seen.insert(loc.binding_key()));
    locations
}

pub async fn create_locator(config: &LocatorConfig) -> Result<Box<dyn Locator>> {
    match config {
        LocatorConfig::Memory | LocatorConfig::Http { .. } => {
            Ok(Box::new(MemoryLocator::new()) as Box<dyn Locator>)
        }
        LocatorConfig::Database { url } => {
            let db_locator = DbLocator::new(url.clone()).await?;
            Ok(Box::new(db_locator) as Box<dyn Locator>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsip::HostWithPort;
    use rsip::transport::Transport;
    use rsipstack::transport::SipAddr;
    use std::time::Duration;

    #[tokio::test]
    async fn memory_locator_orders_by_last_modified() {
        let locator = MemoryLocator::new();
        let uri: rsip::Uri = "sip:alice@rustpbx.com".try_into().unwrap();

        let now = Instant::now();
        let older = now - Duration::from_secs(120);

        let destination_primary = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("127.0.0.1:5060").unwrap(),
        };

        let destination_secondary = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("127.0.0.1:5070").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("rustpbx.com"),
                Location {
                    aor: uri.clone(),
                    expires: 3600,
                    destination: Some(destination_secondary),
                    last_modified: Some(older),
                    instance_id: Some("secondary".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        locator
            .register(
                "alice",
                Some("rustpbx.com"),
                Location {
                    aor: uri.clone(),
                    expires: 3600,
                    destination: Some(destination_primary),
                    last_modified: Some(now),
                    instance_id: Some("primary".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&uri).await.unwrap();
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].instance_id.as_deref(), Some("primary"));
        assert_eq!(locations[1].instance_id.as_deref(), Some("secondary"));
    }

    #[tokio::test]
    async fn memory_locator_matches_localhost_alias() {
        let locator = MemoryLocator::new();
        let registered_uri: rsip::Uri = "sip:alice@192.168.3.181".try_into().unwrap();
        let lookup_uri: rsip::Uri = "sip:alice@localhost".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.168.3.181:5060").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("192.168.3.181"),
                Location {
                    aor: registered_uri.clone(),
                    registered_aor: Some(registered_uri.clone()),
                    expires: 3600,
                    destination: Some(destination),
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].aor.to_string(), registered_uri.to_string());
    }

    #[test]
    fn test_is_local_realm_logic() {
        assert!(is_local_realm("localhost"));
        assert!(is_local_realm("127.0.0.1"));
        assert!(is_local_realm("0.0.0.0"));
        assert!(is_local_realm("::1"));
        assert!(is_local_realm("192.168.1.1"));
        assert!(is_local_realm("10.0.0.1"));
        assert!(is_local_realm("172.16.0.1"));
        assert!(is_local_realm("[::1]"));
        assert!(is_local_realm("127.0.0.1:5060"));

        assert!(!is_local_realm("rustpbx.com"));
        assert!(!is_local_realm("8.8.8.8"));
        assert!(!is_local_realm(""));
    }

    #[tokio::test]
    async fn test_realm_matches_logic() {
        let locator = MemoryLocator::new();
        // Both local
        assert!(realm_matches(&locator, "127.0.0.1", "localhost").await);
        assert!(realm_matches(&locator, "192.168.1.1", "10.0.0.1").await);

        // One local, one not
        assert!(!realm_matches(&locator, "127.0.0.1", "rustpbx.com").await);
        assert!(!realm_matches(&locator, "rustpbx.com", "127.0.0.1").await);

        // Both same non-local
        assert!(realm_matches(&locator, "rustpbx.com", "rustpbx.com").await);
        assert!(realm_matches(&locator, "rustpbx.com:5060", "rustpbx.com").await);

        // Different non-local
        assert!(!realm_matches(&locator, "rustpbx.com", "other.com").await);
    }

    #[tokio::test]
    async fn test_custom_realm_checker() {
        let locator = MemoryLocator::new();
        // Custom checker that treats "my-special-realm.com" as local
        locator.set_realm_checker(Arc::new(|realm| {
            let is_special = realm == "my-special-realm.com";
            let realm = realm.to_string();
            Box::pin(async move { is_special || is_local_realm(&realm) })
        }));

        let registered_uri: rsip::Uri = "sip:alice@my-special-realm.com".try_into().unwrap();
        let lookup_uri: rsip::Uri = "sip:alice@localhost".try_into().unwrap();

        locator
            .register(
                "alice",
                Some("my-special-realm.com"),
                Location {
                    aor: registered_uri.clone(),
                    expires: 3600,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(locations.len(), 1);
    }

    #[tokio::test]
    async fn test_uri_matches_relaxed() {
        let locator = MemoryLocator::new();
        let registered_uri: rsip::Uri = "sip:3sf0hatf@eee3se8lru7o.invalid".try_into().unwrap();
        let lookup_uri: rsip::Uri = "sip:3sf0hatf@eee3se8lru7o.invalid;transport=ws"
            .try_into()
            .unwrap();

        locator
            .register(
                "test_user",
                Some("localhost"),
                Location {
                    aor: registered_uri.clone(),
                    expires: 3600,
                    transport: Some(Transport::Ws),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(locations.len(), 1);

        let lookup_uri_no_transport: rsip::Uri =
            "sip:3sf0hatf@eee3se8lru7o.invalid".try_into().unwrap();
        let locations = locator.lookup(&lookup_uri_no_transport).await.unwrap();
        assert_eq!(locations.len(), 1);
    }
}
