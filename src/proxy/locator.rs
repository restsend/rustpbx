use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transport::SipAddr;
use std::{collections::HashMap, time::Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Location {
    pub aor: rsip::Uri,
    pub expires: u32,
    pub destination: SipAddr,
    pub last_modified: Instant,
}

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
            Err(anyhow::anyhow!("User not found"))
        }
    }
}
