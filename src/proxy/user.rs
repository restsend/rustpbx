use super::{user_db::DbBackend, user_http::HttpUserBackend, user_plain::PlainTextBackend};
use crate::{
    call::user::SipUser,
    config::{ProxyConfig, UserBackendConfig},
    proxy::user_db::DbBackendConfig,
};
use anyhow::Result;
use async_trait::async_trait;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use tracing::info;

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
            let db_config = DbBackendConfig {
                table_name: table_name.clone().unwrap_or_else(|| "users".to_string()),
                username_column: username_column
                    .clone()
                    .unwrap_or_else(|| "username".to_string()),
                password_column: password_column
                    .clone()
                    .unwrap_or_else(|| "password".to_string()),
                enabled_column: enabled_column.clone(),
                id_column: id_column.clone(),
                realm_column: realm_column.clone(),
                display_name_column: None,
                email_column: None,
                phone_column: None,
                note_column: None,
                deleted_at_column: None,
            };
            let backend = DbBackend::new(url.clone(), db_config).await?;
            Ok(Box::new(backend))
        }
    }
}
