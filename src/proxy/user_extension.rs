use crate::call::user::SipUser;
use crate::models::{department, extension};
use anyhow::Result;
use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use super::user::UserBackend;

pub struct ExtensionUserBackend {
    db: DatabaseConnection,
}

impl ExtensionUserBackend {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn connect(database_url: &str) -> Result<Self> {
        let db = crate::models::create_db(database_url).await?;
        Ok(Self::new(db))
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
            Some(departments.into_iter().map(|d| d.name).collect())
        };

        SipUser {
            id: model.id as u64,
            username: model.extension,
            password: model.sip_password,
            enabled: !model.login_disabled,
            realm: realm.map(|r| r.to_string()),
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

    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<Option<SipUser>> {
        if username.trim().is_empty() {
            return Ok(None);
        }

        let result = self.fetch_extension(username).await?;
        let Some((model, departments)) = result else {
            return Ok(None);
        };

        let user = Self::build_sip_user(model, departments, realm);
        Ok(Some(user))
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

        let backend = ExtensionUserBackend::new(db.clone());

        let user = backend
            .get_user("1001", Some("rustpbx.com"))
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
        let backend = ExtensionUserBackend::new(db);
        assert!(backend.get_user("2001", None).await.unwrap().is_none());
    }
}
