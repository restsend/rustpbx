use super::locator_db::DbLocator;
use crate::{
    call::Location,
    config::{LocatorConfig, ProxyConfig},
};
use anyhow::Result;
use async_trait::async_trait;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;

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
    async fn lookup(&self, username: &str, realm: Option<&str>) -> Result<Vec<Location>>;
}

pub struct MemoryLocator {
    locations: Mutex<HashMap<String, Location>>,
}

impl MemoryLocator {
    pub fn new() -> Self {
        Self {
            locations: Mutex::new(HashMap::new()),
        }
    }
    pub fn create(
        _config: Arc<ProxyConfig>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn Locator>>> + Send>> {
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

    async fn lookup(&self, username: &str, realm: Option<&str>) -> Result<Vec<Location>> {
        let identifier = self.get_identifier(username, realm);
        let locations = self.locations.lock().await;
        if let Some(location) = locations.get(&identifier) {
            Ok(vec![location.clone()])
        } else {
            Err(anyhow::anyhow!("missing user: {}", identifier))
        }
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
