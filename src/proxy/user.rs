use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::HeadersExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SipUser {
    pub id: u64,
    pub username: String,
    pub password: Option<String>,
    pub enabled: bool,
    pub realm: Option<String>,
}

impl SipUser {}

impl TryFrom<&rsip::Request> for SipUser {
    type Error = rsipstack::Error;

    fn try_from(req: &rsip::Request) -> Result<Self, Self::Error> {
        let username = req
            .from_header()?
            .uri()?
            .user()
            .unwrap_or_default()
            .to_string();
        let realm = req.to_header()?.uri()?.host().to_string();

        Ok(SipUser {
            id: 0,
            username,
            password: None,
            enabled: true,
            realm: Some(realm),
        })
    }
}

#[async_trait]
pub trait UserBackend: Send + Sync {
    fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        if let Some(realm) = realm {
            format!("{}@{}", user, realm)
        } else {
            user.to_string()
        }
    }
    async fn create_user(&self, user: SipUser) -> Result<()>;
    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser>;
    async fn update_user(&self, username: &str, realm: Option<&str>, user: SipUser) -> Result<()>;
    async fn delete_user(&self, username: &str, realm: Option<&str>) -> Result<()>;
    async fn update_user_password(
        &self,
        username: &str,
        realm: Option<&str>,
        password: &str,
    ) -> Result<()>;
    async fn enable_user(&self, username: &str, realm: Option<&str>) -> Result<()>;
    async fn disable_user(&self, username: &str, realm: Option<&str>) -> Result<()>;
}

pub struct MemoryUserBackend {
    users: Mutex<HashMap<String, SipUser>>,
}

impl MemoryUserBackend {
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
        }
    }
}
#[async_trait]
impl UserBackend for MemoryUserBackend {
    async fn create_user(&self, user: SipUser) -> Result<()> {
        let identifier = self.get_identifier(user.username.as_str(), user.realm.as_deref());
        self.users.lock().await.insert(identifier, user);
        Ok(())
    }

    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let identifier = self.get_identifier(username, realm);
        self.users
            .lock()
            .await
            .get(&identifier)
            .cloned()
            .ok_or(anyhow::anyhow!("User not found"))
    }

    async fn update_user(&self, username: &str, realm: Option<&str>, user: SipUser) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        self.users.lock().await.insert(identifier, user);
        Ok(())
    }

    async fn delete_user(&self, user: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(user, realm);
        self.users.lock().await.remove(&identifier);
        Ok(())
    }

    async fn update_user_password(
        &self,
        username: &str,
        realm: Option<&str>,
        password: &str,
    ) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let mut users = self.users.lock().await;
        let user = users.get_mut(&identifier).unwrap();
        user.password = Some(password.to_string());
        Ok(())
    }

    async fn enable_user(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let mut users = self.users.lock().await;
        let user = users.get_mut(&identifier).unwrap();
        user.enabled = true;
        Ok(())
    }

    async fn disable_user(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let mut users = self.users.lock().await;
        let user = users.get_mut(&identifier).unwrap();
        user.enabled = false;
        Ok(())
    }
}
