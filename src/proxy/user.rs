use super::{user_db::DbBackend, user_http::HttpUserBackend, user_plain::PlainTextBackend};
use crate::config::{ProxyConfig, UserBackendConfig};
use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::HeadersExt;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
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
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        realm: Option<&str>,
    ) -> Result<bool>;
    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser>;
    async fn create_user(&self, _user: SipUser) -> Result<()> {
        Ok(())
    }
}

pub struct MemoryUserBackend {
    users: Mutex<HashMap<String, SipUser>>,
}

impl MemoryUserBackend {
    pub fn create(
        _config: Arc<ProxyConfig>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn UserBackend>>> + Send>> {
        Box::pin(async move { Ok(Box::new(MemoryUserBackend::new()) as Box<dyn UserBackend>) })
    }

    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
        }
    }
    pub async fn create_user(&self, user: SipUser) -> Result<()> {
        let identifier = self.get_identifier(user.username.as_str(), user.realm.as_deref());
        self.users.lock().await.insert(identifier, user);
        Ok(())
    }

    pub async fn update_user(
        &self,
        username: &str,
        realm: Option<&str>,
        user: SipUser,
    ) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        self.users.lock().await.insert(identifier, user);
        Ok(())
    }

    pub async fn delete_user(&self, user: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(user, realm);
        self.users.lock().await.remove(&identifier);
        Ok(())
    }

    pub async fn update_user_password(
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

    pub async fn enable_user(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let mut users = self.users.lock().await;
        let user = users.get_mut(&identifier).unwrap();
        user.enabled = true;
        Ok(())
    }

    pub async fn disable_user(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let mut users = self.users.lock().await;
        let user = users.get_mut(&identifier).unwrap();
        user.enabled = false;
        Ok(())
    }
}
#[async_trait]
impl UserBackend for MemoryUserBackend {
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        _realm: Option<&str>,
    ) -> Result<bool> {
        let identifier = self.get_identifier(username, None);
        if let Some(user) = self.users.lock().await.get(&identifier) {
            match user.password {
                Some(ref stored_password) => return Ok(stored_password == password),
                None => return Err(anyhow::anyhow!("Password not set for user")),
            }
        }
        return Err(anyhow::anyhow!("User not found"));
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
}

pub async fn create_user_backend(config: &UserBackendConfig) -> Result<Box<dyn UserBackend>> {
    match config {
        UserBackendConfig::Http {
            url,
            method,
            username_field,
            password_field,
            realm_field,
            headers,
        } => {
            let backend = HttpUserBackend::new(
                url,
                method,
                username_field,
                password_field,
                realm_field,
                headers,
            );
            Ok(Box::new(backend))
        }

        UserBackendConfig::Memory => Ok(Box::new(MemoryUserBackend::new())),
        UserBackendConfig::Plain { path } => {
            let backend = PlainTextBackend::new(path);
            backend.load().await?;
            Ok(Box::new(backend))
        }
        UserBackendConfig::Database {
            url,
            table_name,
            username_column,
            password_column,
            enabled_column,
            password_hash,
            password_salt,
            id_column,
            realm_column,
        } => {
            let backend = DbBackend::new(
                url.clone(),
                table_name.clone(),
                id_column.clone(),
                username_column.clone(),
                password_column.clone(),
                enabled_column.clone(),
                realm_column.clone(),
                password_hash.clone(),
                password_salt.clone(),
            )
            .await?;
            Ok(Box::new(backend))
        }
    }
}
