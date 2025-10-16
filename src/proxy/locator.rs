use super::locator_db::DbLocator;
use crate::{
    call::Location,
    config::{LocatorConfig, ProxyConfig},
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::{transaction::endpoint::TargetLocator, transport::SipAddr};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tracing::debug;

type LocatorCreationFuture = Pin<Box<dyn Future<Output = Result<Box<dyn Locator>>> + Send>>;

#[async_trait]
pub trait Locator: Send + Sync {
    fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        if let Some(realm) = realm {
            format!("{}@{}", user, realm)
        } else {
            user.to_string()
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
    locations: Mutex<HashMap<String, Location>>,
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
        debug!(identifier, %location, "Registering");
        let mut locations = self.locations.lock().await;
        locations.insert(identifier, location);
        Ok(())
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let mut locations = self.locations.lock().await;
        locations.remove(&identifier);
        Ok(())
    }

    async fn lookup(&self, uri: &rsip::Uri) -> Result<Vec<Location>> {
        let username = uri.user().unwrap_or_else(|| "");
        let realm = uri.host().to_string();

        let identifiers = vec![
            self.get_identifier(username, Some(realm.as_str())),
            self.get_identifier(username, Some("localhost")),
            self.get_identifier(username, None),
        ];

        let locations = self.locations.lock().await;
        for id in identifiers {
            if let Some(location) = locations.get(&id).cloned() {
                return Ok(vec![location]);
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
