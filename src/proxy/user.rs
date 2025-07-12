use super::{user_db::DbBackend, user_http::HttpUserBackend, user_plain::PlainTextBackend};
use crate::{
    config::{ProxyConfig, UserBackendConfig},
    proxy::auth::check_authorization_headers,
};
use anyhow::Result;
use async_trait::async_trait;
use rsip::{
    headers::auth::Algorithm,
    prelude::{HeadersExt, ToTypedHeader},
};
use rsipstack::{
    transaction::transaction::Transaction,
    transport::{SipAddr, SipConnection},
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
    #[serde(skip)]
    pub destination: Option<SipAddr>,
    #[serde(default = "default_is_support_webrtc")]
    pub is_support_webrtc: bool,
}

fn default_enabled() -> bool {
    true
}

fn default_is_support_webrtc() -> bool {
    false
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
            destination: None,
            is_support_webrtc: false,
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

    pub fn build_contact_from_invite(&self, tx: &Transaction) -> Option<rsip::typed::Contact> {
        let addr = match tx.endpoint_inner.get_addrs().first() {
            Some(addr) => addr.clone(),
            None => return None,
        };

        let mut contact_params = vec![];
        match addr.r#type {
            Some(rsip::Transport::Udp) | None => {}
            Some(t) => {
                contact_params.push(rsip::Param::Transport(t));
            }
        }
        let contact = rsip::typed::Contact {
            display_name: None,
            uri: rsip::Uri {
                scheme: addr.r#type.map(|t| t.sip_scheme()),
                auth: Some(rsip::Auth {
                    user: self.get_contact_username(),
                    password: None,
                }),
                host_with_port: addr.addr.clone(),
                ..Default::default()
            },
            params: contact_params,
        };
        Some(contact)
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

impl TryFrom<&Transaction> for SipUser {
    type Error = anyhow::Error;

    fn try_from(tx: &Transaction) -> Result<Self, Self::Error> {
        let (username, realm) = match check_authorization_headers(&tx.original) {
            Ok(Some((user, _))) => (user.username, user.realm),
            _ => {
                let username = tx
                    .original
                    .from_header()?
                    .uri()?
                    .user()
                    .unwrap_or_default()
                    .to_string();
                let realm = tx.original.to_header()?.uri()?.host().to_string();
                (username, Some(realm))
            }
        };
        let origin_contact = match tx.original.contact_header() {
            Ok(contact) => contact.typed().ok(),
            Err(_) => None,
        };
        // Use rsipstack's via_received functionality to get destination
        let via_header = tx.original.via_header()?;
        let destination_addr = SipConnection::parse_target_from_via(via_header)
            .map_err(|e| anyhow::anyhow!("failed to parse via header: {:?}", e))?;

        let destination = SipAddr {
            r#type: via_header.trasnport().ok(),
            addr: destination_addr,
        };
        let is_support_webrtc = destination
            .r#type
            .is_some_and(|t| t == rsip::transport::Transport::Wss);

        Ok(SipUser {
            id: 0,
            username,
            password: None,
            enabled: true,
            realm,
            origin_contact,
            destination: Some(destination),
            is_support_webrtc,
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
    async fn is_same_realm(&self, realm: &str) -> bool;
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
    async fn is_same_realm(&self, realm: &str) -> bool {
        return realm.is_empty();
    }

    fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        Self::get_identifier(user, realm)
    }

    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let users = self.users.lock().await;
        let identifier = self.get_identifier(username, realm);
        let mut user = match users.get(&identifier) {
            Some(user) => user.clone(),
            None => {
                match users.get(username) {
                    Some(user) => {
                        if user.realm.as_ref().is_some_and(|r| !r.is_empty()) {
                            return Err(anyhow::anyhow!("missing user: {}", identifier));
                        }
                        return Ok(user.clone());
                    }
                    None => {}
                }
                return Err(anyhow::anyhow!("missing user: {}", identifier));
            }
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
