use super::{user_db::DbBackend, user_http::HttpUserBackend, user_plain::PlainTextBackend};
use crate::{
    config::{ProxyConfig, UserBackendConfig},
    proxy::auth::AuthModule,
};
use anyhow::Result;
use async_trait::async_trait;
use rsip::{
    headers::auth::Algorithm,
    prelude::{HeadersExt, ToTypedHeader},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tracing::info;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SipUser {
    #[serde(default)]
    pub id: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub username: String,
    pub password: Option<String>,
    pub realm: Option<String>,
    #[serde(skip)]
    pub origin_contact: Option<rsip::typed::Contact>,
}
fn default_enabled() -> bool {
    true
}
impl Default for SipUser {
    fn default() -> Self {
        Self {
            id: 0,
            enabled: true,
            username: "".to_string(),
            password: None,
            realm: None,
            origin_contact: None,
        }
    }
}

impl SipUser {
    pub fn get_contact_username(&self) -> String {
        match self.origin_contact {
            Some(ref contact) => contact.uri.user().unwrap_or_default().to_string(),
            None => self.username.clone(),
        }
    }
    pub fn auth_digest(&self, algorithm: Algorithm) -> String {
        use md5::{Digest, Md5};
        use sha2::{Sha256, Sha512};
        let value = format!(
            "{}:{}:{}",
            self.username,
            self.realm.as_ref().unwrap_or(&"".to_string()),
            self.password.as_ref().unwrap_or(&"".to_string()),
        );
        match algorithm {
            Algorithm::Md5 | Algorithm::Md5Sess => {
                let mut hasher = Md5::new();
                hasher.update(value);
                format!("{:x}", hasher.finalize())
            }
            Algorithm::Sha256 | Algorithm::Sha256Sess => {
                let mut hasher = Sha256::new();
                hasher.update(value);
                format!("{:x}", hasher.finalize())
            }
            Algorithm::Sha512 | Algorithm::Sha512Sess => {
                let mut hasher = Sha512::new();
                hasher.update(value);
                format!("{:x}", hasher.finalize())
            }
        }
    }
}

impl TryFrom<&rsip::Request> for SipUser {
    type Error = rsipstack::Error;

    fn try_from(req: &rsip::Request) -> Result<Self, Self::Error> {
        let (username, realm) = match AuthModule::check_authorization_headers(req) {
            Ok(Some((user, _))) => (user.username, user.realm),
            _ => {
                let username = req
                    .from_header()?
                    .uri()?
                    .user()
                    .unwrap_or_default()
                    .to_string();
                let realm = req.to_header()?.uri()?.host().to_string();
                (username, Some(realm))
            }
        };
        let origin_contact = req.contact_header()?.typed().ok();
        Ok(SipUser {
            id: 0,
            username,
            password: None,
            enabled: true,
            realm,
            origin_contact,
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
        config: Arc<ProxyConfig>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn UserBackend>>> + Send>> {
        Box::pin(async move {
            let builtin_users = match &config.user_backend {
                UserBackendConfig::Memory { users } => users.clone(),
                _ => None,
            };
            Ok(Box::new(MemoryUserBackend::new(builtin_users)) as Box<dyn UserBackend>)
        })
    }
    fn get_identifier(user: &str, realm: Option<&str>) -> String {
        if let Some(realm) = realm {
            format!("{}@{}", user, ProxyConfig::normalize_realm(realm))
        } else {
            user.to_string()
        }
    }

    pub fn new(builtin_users: Option<Vec<SipUser>>) -> Self {
        info!(
            "Creating MemoryUserBackend, users: {}",
            builtin_users.as_ref().map(|us| us.len()).unwrap_or(0)
        );
        let mut users = HashMap::new();
        if let Some(builtin_users) = builtin_users {
            for user in builtin_users {
                let identifier =
                    Self::get_identifier(user.username.as_str(), user.realm.as_deref());
                users.insert(identifier, user);
            }
        }
        Self {
            users: Mutex::new(users),
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
    fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        Self::get_identifier(user, realm)
    }

    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let users = self.users.lock().await;
        let identifier = self.get_identifier(username, realm);
        let mut user = match users.get(&identifier) {
            Some(user) => user.clone(),
            None => return Err(anyhow::anyhow!("missing user: {}", identifier)),
        };
        user.realm = realm.map(|r| r.to_string());
        Ok(user)
    }

    async fn create_user(&self, user: SipUser) -> Result<()> {
        let identifier = self.get_identifier(user.username.as_str(), user.realm.as_deref());
        self.users.lock().await.insert(identifier, user);
        Ok(())
    }
}

pub async fn create_user_backend(config: &UserBackendConfig) -> Result<Box<dyn UserBackend>> {
    match config {
        UserBackendConfig::Http {
            url,
            method,
            username_field,
            realm_field,
            headers,
        } => {
            let backend = HttpUserBackend::new(url, method, username_field, realm_field, headers);
            Ok(Box::new(backend))
        }
        UserBackendConfig::Memory { users } => Ok(Box::new(MemoryUserBackend::new(users.clone()))),
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
            )
            .await?;
            Ok(Box::new(backend))
        }
    }
}
