use crate::call::user::SipUser;
use crate::models::{department, extension};
use crate::proxy::auth::AuthError;
use anyhow::Result;
use async_trait::async_trait;
use lru::LruCache;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::user::UserBackend;

pub struct ExtensionUserBackend {
    db: DatabaseConnection,
    cache: Arc<Mutex<LruCache<(String, Option<String>), (Option<SipUser>, Instant)>>>,
    ttl: Duration,
}

impl ExtensionUserBackend {
    pub fn new(db: DatabaseConnection, ttl_secs: u64) -> Self {
        Self {
            db,
            cache: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(10000).unwrap()))),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    pub async fn connect(database_url: &str, ttl_secs: u64) -> Result<Self> {
        let db = crate::models::create_db(database_url).await?;
        Ok(Self::new(db, ttl_secs))
    }

    async fn fetch_extension(
        &self,
        ext: &str,
    ) -> Result<Option<(extension::Model, Vec<department::Model>)>> {
        let mut results = extension::Entity::find()
            .filter(extension::Column::Extension.eq(ext))
            .find_with_related(department::Entity)
            .all(&self.db)
            .await?;

        Ok(results.pop())
    }

    fn build_sip_user(
        model: extension::Model,
        departments: Vec<department::Model>,
        realm: Option<&str>,
    ) -> SipUser {
        let department_names = if departments.is_empty() {
            None
        } else {
            let deps: Vec<String> = departments.iter().map(|d| d.name.clone()).collect();
            Some(deps)
        };

        SipUser {
            id: model.id as u64,
            username: model.extension,
            password: model.sip_password,
            enabled: !model.login_disabled,
            realm: realm.map(|r| r.to_string()),
            call_forwarding_mode: model.call_forwarding_mode,
            call_forwarding_destination: model.call_forwarding_destination,
            call_forwarding_timeout: model.call_forwarding_timeout,
            departments: department_names,
            display_name: model.display_name,
            email: model.email,
            note: model.notes,
            allow_guest_calls: model.allow_guest_calls,
            ..Default::default()
        }
    }
}

#[async_trait]
impl UserBackend for ExtensionUserBackend {
    async fn is_same_realm(&self, realm: &str) -> bool {
        realm.is_empty()
    }

    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        _request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        if username.trim().is_empty() {
            return Ok(None);
        }

        let cache_key = (username.to_string(), realm.map(|r| r.to_string()));

        // Check cache
        if self.ttl.as_secs() > 0 {
            let mut cache = self.cache.lock().unwrap();
            if let Some((user, timestamp)) = cache.get(&cache_key) {
                if timestamp.elapsed() < self.ttl {
                    return Ok(user.clone());
                }
            }
        }

        let result = self
            .fetch_extension(username)
            .await
            .map_err(AuthError::from)?;
        let user = if let Some((model, departments)) = result {
            Some(Self::build_sip_user(model, departments, realm))
        } else {
            None
        };

        // Update cache
        if self.ttl.as_secs() > 0 {
            let mut cache = self.cache.lock().unwrap();
            cache.put(cache_key, (user.clone(), Instant::now()));
        }

        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::migration::Migrator;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database};
    use sea_orm_migration::MigratorTrait;

    async fn setup_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        Migrator::up(&db, None)
            .await
            .expect("migrations should succeed");
        db
    }

    #[tokio::test]
    async fn get_user_returns_extension() {
        let db = setup_db().await;

        let _ = extension::ActiveModel {
            extension: Set("1001".to_string()),
            sip_password: Set(Some("secret".to_string())),
            allow_guest_calls: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert extension");

        let backend = ExtensionUserBackend::new(db.clone(), 30);

        let user = backend
            .get_user("1001", Some("rustpbx.com"), None)
            .await
            .expect("query user")
            .expect("user exists");

        assert_eq!(user.username, "1001");
        assert_eq!(user.password.as_deref(), Some("secret"));
        assert!(user.allow_guest_calls);
        assert_eq!(user.realm.as_deref(), Some("rustpbx.com"));
        assert!(user.enabled);
    }

    #[tokio::test]
    async fn missing_user_returns_none() {
        let db = setup_db().await;
        let backend = ExtensionUserBackend::new(db, 30);
        assert!(backend.get_user("2001", None, None).await.unwrap().is_none());
    }
}
