use super::user::UserBackend;
use crate::{call::user::SipUser, proxy::auth::AuthError};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use sqlx::{AnyPool, Row};

pub struct DbBackendConfig {
    pub table_name: String,
    pub username_column: String,
    pub password_column: String,
    pub id_column: Option<String>,
    pub enabled_column: Option<String>,
    pub realm_column: Option<String>,
    pub display_name_column: Option<String>,
    pub email_column: Option<String>,
    pub phone_column: Option<String>,
    pub note_column: Option<String>,
    pub deleted_at_column: Option<String>,
}

impl Default for DbBackendConfig {
    fn default() -> Self {
        Self {
            table_name: "users".to_string(),
            username_column: "username".to_string(),
            password_column: "password".to_string(),
            id_column: None,
            enabled_column: None,
            realm_column: None,
            display_name_column: None,
            email_column: None,
            phone_column: None,
            note_column: None,
            deleted_at_column: None,
        }
    }
}
pub struct DbBackend {
    db: AnyPool,
    config: DbBackendConfig,
}

impl DbBackend {
    pub async fn new(url: String, config: DbBackendConfig) -> Result<Self> {
        let db = sqlx::any::AnyPoolOptions::new()
            .connect(&url)
            .await
            .map_err(|e| anyhow!("Database connection error: {}", e))?;

        Ok(Self { db, config })
    }
}

#[async_trait]
impl UserBackend for DbBackend {
    async fn is_same_realm(&self, realm: &str) -> bool {
        let host = if let Some(pos) = realm.find(':') {
            &realm[..pos]
        } else {
            realm
        };

        if let Some(ref realm_col) = self.config.realm_column {
            let query = format!(
                "SELECT COUNT(*) FROM {} WHERE {} = ? OR {} = ?",
                self.config.table_name, realm_col, realm_col
            );
            let count = sqlx::query(&query)
                .bind(realm)
                .bind(host)
                .fetch_one(&self.db)
                .await
                .map_err(|e| anyhow!("Database query error: {}", e))
                .map(|row| row.get::<i64, _>(0))
                .unwrap_or(0);
            return count > 0;
        }
        false
    }
    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        _request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        // Build SELECT clause with optional columns
        let mut select_columns = vec![
            self.config.username_column.clone(),
            self.config.password_column.clone(),
        ];

        if let Some(ref id_col) = self.config.id_column {
            select_columns.push(id_col.clone());
        }

        if let Some(ref enabled_col) = self.config.enabled_column {
            select_columns.push(enabled_col.clone());
        }

        if let Some(ref realm_col) = self.config.realm_column {
            select_columns.push(realm_col.clone());
        }

        let select_clause = select_columns.join(", ");

        // Build WHERE clause
        let mut where_clause = format!("{} = ?", self.config.username_column);
        let mut bind_params: Vec<&str> = vec![username];

        if let Some(realm) = realm {
            if let Some(ref realm_col) = self.config.realm_column {
                let host = if let Some(pos) = realm.find(':') {
                    &realm[..pos]
                } else {
                    realm
                };
                if host != realm {
                    where_clause
                        .push_str(&format!(" AND ({} = ? OR {} = ?)", realm_col, realm_col));
                    bind_params.push(realm);
                    bind_params.push(host);
                } else {
                    where_clause.push_str(&format!(" AND {} = ?", realm_col));
                    bind_params.push(realm);
                }
            }
        }
        if let Some(ref deleted_at_col) = self.config.deleted_at_column {
            where_clause.push_str(&format!(" AND {} IS NULL", deleted_at_col));
        }

        let query = format!(
            "SELECT {} FROM {} WHERE {}",
            select_clause, self.config.table_name, where_clause
        );

        let mut sqlx_query = sqlx::query(&query);
        for param in bind_params {
            sqlx_query = sqlx_query.bind(param);
        }

        let row = match sqlx_query.fetch_one(&self.db).await {
            Ok(row) => row,
            Err(e) => {
                if let sqlx::Error::RowNotFound = e {
                    return Ok(None);
                }
                return Err(AuthError::Other(e.into()));
            }
        };

        // Map the database row to a SipUser
        let id: i64 = if let Some(ref id_col) = self.config.id_column {
            row.try_get(id_col.as_str()).unwrap_or(0)
        } else {
            0
        };

        let db_username: String = row
            .try_get(self.config.username_column.as_str())
            .unwrap_or_default();

        let password: String = row
            .try_get(self.config.password_column.as_str())
            .unwrap_or_default();

        let enabled: bool = if let Some(ref enabled_col) = self.config.enabled_column {
            row.try_get(enabled_col.as_str()).unwrap_or(true)
        } else {
            true
        };

        let db_realm: Option<String> = if let Some(ref realm_col) = self.config.realm_column {
            row.try_get(realm_col.as_str()).ok()
        } else {
            None
        };

        let display_name = self
            .config
            .display_name_column
            .as_ref()
            .and_then(|k| row.try_get(k.as_str()).ok());

        let email = self
            .config
            .email_column
            .as_ref()
            .and_then(|k| row.try_get(k.as_str()).ok());
        let phone = self
            .config
            .phone_column
            .as_ref()
            .and_then(|k| row.try_get(k.as_str()).ok());
        let note = self
            .config
            .note_column
            .as_ref()
            .and_then(|k| row.try_get(k.as_str()).ok());

        Ok(Some(SipUser {
            id: id as u64,
            username: db_username,
            password: Some(password),
            enabled,
            realm: realm.map(|r| r.to_string()).or(db_realm),
            origin_contact: None,
            contact: None,
            from: None,
            destination: None,
            is_support_webrtc: false,
            call_forwarding_mode: None,
            call_forwarding_destination: None,
            call_forwarding_timeout: None,
            departments: None,
            display_name,
            email,
            phone,
            note,
            allow_guest_calls: false,
        }))
    }
}
