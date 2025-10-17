use super::locator_db::DbLocator;
use crate::{
    call::Location,
    config::{LocatorConfig, ProxyConfig},
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::{transaction::endpoint::TargetLocator, transport::SipAddr};
use std::{
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
            let mut seen = HashSet::new();
            let unique = direct_hits
                .into_iter()
                .filter(|loc| seen.insert(loc.binding_key()))
                .collect();
            return Ok(unique);
        }

        // Fall back to classic AoR lookup by username/realm
        let username_raw = uri.user().unwrap_or_else(|| "");
        let username = username_raw.trim();
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
                    return Ok(map.values().cloned().collect());
                }
            }
        }

        if !username.is_empty() {
            let mut fallback_hits = Vec::new();

            for map in locations.values() {
                for loc in map.values() {
                    let mut matched = false;

                    if let Some(registered) = &loc.registered_aor {
                        if registered
                            .user()
                            .map(|u| u.trim().eq_ignore_ascii_case(username))
                            .unwrap_or(false)
                        {
                            matched = true;
                        }
                    }

                    if !matched
                        && loc
                            .aor
                            .user()
                            .map(|u| u.trim().eq_ignore_ascii_case(username))
                            .unwrap_or(false)
                    {
                        matched = true;
                    }

                    if matched {
                        fallback_hits.push(loc.clone());
                    }
                }
            }

            if !fallback_hits.is_empty() {
                let mut seen = HashSet::new();
                let unique = fallback_hits
                    .into_iter()
                    .filter(|loc| seen.insert(loc.binding_key()))
                    .collect();
                return Ok(unique);
            }
        }

        Ok(vec![])
    }
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
