use super::{
    user_db::DbBackend, user_extension::ExtensionUserBackend, user_http::HttpUserBackend,
    user_plain::PlainTextBackend,
};
use crate::{
    call::user::SipUser,
    config::{Config, ProxyConfig, UserBackendConfig},
    proxy::{auth::AuthError, user_db::DbBackendConfig},
};
use anyhow::{Result, anyhow};
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
    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError>;
    async fn create_user(&self, _user: SipUser) -> Result<()> {
        Ok(())
    }
}

pub struct MemoryUserBackend {
    users: Mutex<HashMap<String, SipUser>>,
}

// Type alias to simplify complex return type
type UserBackendFuture = Pin<Box<dyn Future<Output = Result<Box<dyn UserBackend>>> + Send>>;

impl MemoryUserBackend {
    pub fn create(config: Arc<ProxyConfig>) -> UserBackendFuture {
        Box::pin(async move {
            let builtin_users = config.user_backends.iter().find_map(|backend| {
                if let UserBackendConfig::Memory { users } = backend {
                    users.clone()
                } else {
                    None
                }
            });
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

    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        _request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        let users = self.users.lock().await;
        let identifier = self.get_identifier(username, realm);
        let mut user = match users.get(&identifier) {
            Some(user) => user.clone(),
            None => {
                if let Some(user) = users.get(username) {
                    if user.realm.as_ref().is_some_and(|r| !r.is_empty()) {
                        return Err(AuthError::NotFound);
                    }
                    return Ok(Some(user.clone()));
                }
                return Ok(None);
            }
        };
        user.realm = realm.map(|r| r.to_string());
        Ok(Some(user))
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
            sip_headers,
        } => {
            let backend = HttpUserBackend::new(
                url,
                method,
                username_field,
                realm_field,
                headers,
                sip_headers,
            );
            Ok(Box::new(backend) as Box<dyn UserBackend>)
        }
        UserBackendConfig::Memory { users } => {
            Ok(Box::new(MemoryUserBackend::new(users.clone())) as Box<dyn UserBackend>)
        }
        UserBackendConfig::Plain { path } => {
            let backend = PlainTextBackend::new(path);
            backend.load().await?;
            Ok(Box::new(backend) as Box<dyn UserBackend>)
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
            let backend = DbBackend::new(
                url.clone().ok_or(anyhow!("database url is required"))?,
                db_config,
            )
            .await?;
            Ok(Box::new(backend) as Box<dyn UserBackend>)
        }
        UserBackendConfig::Extension { database_url, ttl } => {
            let url = database_url
                .clone()
                .unwrap_or_else(|| Config::default().database_url);
            let ttl_secs = ttl.unwrap_or(30); // Default to 30s as requested
            let backend = ExtensionUserBackend::connect(&url, ttl_secs).await?;
            Ok(Box::new(backend) as Box<dyn UserBackend>)
        }
    }
}

pub struct ChainedUserBackend {
    backends: Vec<Box<dyn UserBackend>>,
}

impl ChainedUserBackend {
    pub fn new(backends: Vec<Box<dyn UserBackend>>) -> Self {
        Self { backends }
    }
}

#[async_trait]
impl UserBackend for ChainedUserBackend {
    async fn is_same_realm(&self, realm: &str) -> bool {
        for backend in &self.backends {
            if backend.is_same_realm(realm).await {
                return true;
            }
        }
        false
    }

    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        let mut last_err: Option<AuthError> = None;
        for backend in &self.backends {
            match backend.get_user(username, realm, request).await {
                Ok(Some(user)) => return Ok(Some(user)),
                Ok(None) => {}
                Err(err) => last_err = Some(err),
            }
        }
        if let Some(err) = last_err {
            Err(err)
        } else {
            Ok(None)
        }
    }

    async fn create_user(&self, user: SipUser) -> Result<()> {
        let mut last_err: Option<anyhow::Error> = None;
        for backend in &self.backends {
            match backend.create_user(user.clone()).await {
                Ok(()) => return Ok(()),
                Err(err) => last_err = Some(err),
            }
        }
        if let Some(err) = last_err {
            Err(err)
        } else {
            Ok(())
        }
    }
}

pub async fn build_user_backend(config: &ProxyConfig) -> Result<Box<dyn UserBackend>> {
    if config.user_backends.is_empty() {
        return Err(anyhow!("proxy.user_backends must not be empty"));
    }

    let mut instances = Vec::with_capacity(config.user_backends.len());
    for backend_config in &config.user_backends {
        let backend = create_user_backend(backend_config).await?;
        instances.push(backend);
    }
    Ok(Box::new(ChainedUserBackend::new(instances)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_user_backend_realm() {
        let backend = MemoryUserBackend::new(None);
        let user = SipUser {
            username: "alice".to_string(),
            realm: Some("example.com".to_string()),
            ..Default::default()
        };
        backend.create_user(user).await.unwrap();

        // Test with exact realm
        let found = backend
            .get_user("alice", Some("example.com"), None)
            .await
            .unwrap();
        assert!(found.is_some());

        // Test with realm including port
        let found = backend
            .get_user("alice", Some("example.com:5060"), None)
            .await
            .unwrap();
        assert!(found.is_some());

        // Test with wrong realm
        let found = backend
            .get_user("alice", Some("other.com"), None)
            .await
            .unwrap();
        assert!(found.is_none());
    }
}
