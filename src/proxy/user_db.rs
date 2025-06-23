use super::user::{SipUser, UserBackend};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use sqlx::{AnyPool, Row};

pub struct DbBackend {
    db: AnyPool,
    table_name: String,
    id_column: Option<String>,
    username_column: String,
    password_column: String,
    enabled_column: Option<String>,
    realm_column: Option<String>,
}

impl DbBackend {
    pub async fn new(
        url: String,
        table_name: Option<String>,
        id_column: Option<String>,
        username_column: Option<String>,
        password_column: Option<String>,
        enabled_column: Option<String>,
        realm_column: Option<String>,
    ) -> Result<Self> {
        let db = sqlx::any::AnyPoolOptions::new()
            .connect(&url)
            .await
            .map_err(|e| anyhow!("Database connection error: {}", e))?;

        Ok(Self {
            db,
            table_name: table_name.unwrap_or_else(|| "users".to_string()),
            id_column,
            username_column: username_column.unwrap_or_else(|| "username".to_string()),
            password_column: password_column.unwrap_or_else(|| "password".to_string()),
            enabled_column,
            realm_column,
        })
    }
}

#[async_trait]
impl UserBackend for DbBackend {
    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        // Build SELECT clause with optional columns
        let mut select_columns = vec![self.username_column.clone(), self.password_column.clone()];

        if let Some(ref id_col) = self.id_column {
            select_columns.push(id_col.clone());
        }

        if let Some(ref enabled_col) = self.enabled_column {
            select_columns.push(enabled_col.clone());
        }

        if let Some(ref realm_col) = self.realm_column {
            select_columns.push(realm_col.clone());
        }

        let select_clause = select_columns.join(", ");

        // Build WHERE clause
        let mut where_clause = format!("{} = ?", self.username_column);
        let mut bind_params: Vec<&str> = vec![username];

        if let Some(realm) = realm {
            if let Some(ref realm_col) = self.realm_column {
                where_clause.push_str(&format!(" AND {} = ?", realm_col));
                bind_params.push(realm);
            }
        }

        let query = format!(
            "SELECT {} FROM {} WHERE {}",
            select_clause, self.table_name, where_clause
        );

        let mut sqlx_query = sqlx::query(&query);
        for param in bind_params {
            sqlx_query = sqlx_query.bind(param);
        }

        let row = sqlx_query
            .fetch_one(&self.db)
            .await
            .map_err(|e| anyhow!("Database query error: {}", e))?;

        // Map the database row to a SipUser
        let id: i64 = if let Some(ref id_col) = self.id_column {
            row.try_get(id_col.as_str()).unwrap_or(0)
        } else {
            0
        };

        let db_username: String = row
            .try_get(self.username_column.as_str())
            .unwrap_or_default();

        let password: String = row
            .try_get(self.password_column.as_str())
            .unwrap_or_default();

        let enabled: bool = if let Some(ref enabled_col) = self.enabled_column {
            row.try_get(enabled_col.as_str()).unwrap_or(true)
        } else {
            true
        };

        let db_realm: Option<String> = if let Some(ref realm_col) = self.realm_column {
            row.try_get(realm_col.as_str()).ok()
        } else {
            None
        };

        Ok(SipUser {
            id: id as u64,
            username: db_username,
            password: Some(password),
            enabled,
            realm: realm.map(|r| r.to_string()).or(db_realm),
            origin_contact: None,
        })
    }
}
