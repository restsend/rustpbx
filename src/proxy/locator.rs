use super::locator_db::DbLocator;
use crate::{
    call::Location,
    config::{LocatorConfig, ProxyConfig},
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::{transaction::endpoint::TargetLocator, transport::SipAddr};
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

type LocatorCreationFuture = Pin<Box<dyn Future<Output = Result<Box<dyn Locator>>> + Send>>;

#[async_trait]
pub trait Locator: Send + Sync {
    fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        let username = user.trim().to_ascii_lowercase();
        let realm = realm
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .map(|r| r.to_ascii_lowercase());

        match (username.is_empty(), realm) {
            (true, _) => String::new(),
            (false, Some(realm)) => format!("{}@{}", username, realm),
            (false, None) => username,
        }
    }
    async fn register(&self, username: &str, realm: Option<&str>, location: Location)
    -> Result<()>;
    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()>;
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

pub struct MemoryLocator {
    locations: Mutex<HashMap<String, HashMap<String, Location>>>,
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
        }
    }
    pub fn create(_config: Arc<ProxyConfig>) -> LocatorCreationFuture {
        Box::pin(async move { Ok(Box::new(MemoryLocator::new()) as Box<dyn Locator>) })
    }
}

#[async_trait]
impl Locator for MemoryLocator {
    async fn register(
        &self,
        username: &str,
        realm: Option<&str>,
        location: Location,
    ) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
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
        let identifier = self.get_identifier(username, realm);
        let mut locations = self.locations.lock().await;
        locations.remove(&identifier);
        Ok(())
    }

    async fn lookup(&self, uri: &rsip::Uri) -> Result<Vec<Location>> {
        let mut locations = self.locations.lock().await;
        let now = Instant::now();
        let uri_string = uri.to_string();
        let mut direct_hits = Vec::new();

        // Prune expired bindings and attempt direct contact/GRUU matches first
        locations.retain(|_, map| {
            map.retain(|_, loc| !loc.is_expired_at(now));
            !map.is_empty()
        });

        for map in locations.values() {
            for loc in map.values() {
                if &loc.aor == uri {
                    direct_hits.push(loc.clone());
                    continue;
                }
                if let Some(registered) = &loc.registered_aor {
                    if registered == uri {
                        direct_hits.push(loc.clone());
                        continue;
                    }
                }
                if let Some(gruu) = &loc.gruu {
                    if gruu == &uri_string {
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
                identifiers.push(self.get_identifier(username, Some(realm_trimmed)));
            }
            identifiers.push(self.get_identifier(username, Some("localhost")));
            identifiers.push(self.get_identifier(username, None));
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
                        let realm_match = realm_matches(realm_trimmed, realm_string.trim());

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
                        let realm_match = realm_matches(realm_trimmed, realm_string.trim());

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
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            &rest[..end]
        } else {
            rest
        }
    } else {
        trimmed.split(':').next().unwrap_or(trimmed)
    }
}

pub(crate) fn is_local_realm(realm: &str) -> bool {
    if realm.trim().is_empty() {
        return false;
    }
    let host = host_without_port(realm);
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1"
    )
}

fn realm_matches(requested_realm: &str, candidate_realm: &str) -> bool {
    let requested = requested_realm.trim();
    if requested.is_empty() {
        return true;
    }
    if is_local_realm(requested) {
        return true;
    }

    let requested_host = host_without_port(requested).to_ascii_lowercase();
    let candidate_host = host_without_port(candidate_realm).to_ascii_lowercase();
    requested_host == candidate_host
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
        let uri: rsip::Uri = "sip:alice@example.com".try_into().unwrap();

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
                Some("example.com"),
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
                Some("example.com"),
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
}
