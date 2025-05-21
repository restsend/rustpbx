use super::user::{SipUser, UserBackend};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use md5;
use sha1::Digest as Sha1Digest;
use sqlx::{AnyPool, Row};

pub struct DbBackend {
    db: AnyPool,
    table_name: String,
    id_column: Option<String>,
    username_column: String,
    password_column: String,
    enabled_column: Option<String>,
    realm_column: Option<String>,
    password_hash: Option<String>,
    password_salt: Option<String>,
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
        password_hash: Option<String>,
        password_salt: Option<String>,
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
            password_hash,
            password_salt,
        })
    }

    fn hash_password(&self, password: &str) -> String {
        match self
            .password_hash
            .as_ref()
            .unwrap_or(&"".to_string())
            .as_str()
        {
            "md5" => {
                let mut hasher = md5::Context::new();
                hasher.consume(format!(
                    "{}{}",
                    password,
                    self.password_salt.as_ref().unwrap_or(&"".to_string())
                ));
                format!("{:x}", hasher.compute())
            }
            "sha1" => {
                let mut hasher = sha1::Sha1::new();
                hasher.update(
                    format!(
                        "{}{}",
                        password,
                        self.password_salt.as_ref().unwrap_or(&"".to_string())
                    )
                    .as_bytes(),
                );
                format!("{:x}", hasher.finalize())
            }
            "sha256" => {
                let mut hasher = sha2::Sha256::new();
                hasher.update(
                    format!(
                        "{}{}",
                        password,
                        self.password_salt.as_ref().unwrap_or(&"".to_string())
                    )
                    .as_bytes(),
                );
                format!("{:x}", hasher.finalize())
            }
            "sha512" => {
                let mut hasher = sha2::Sha512::new();
                hasher.update(
                    format!(
                        "{}{}",
                        password,
                        self.password_salt.as_ref().unwrap_or(&"".to_string())
                    )
                    .as_bytes(),
                );
                format!("{:x}", hasher.finalize())
            }
            _ => password.to_string(),
        }
    }
}

#[async_trait]
impl UserBackend for DbBackend {
    async fn authenticate(
        &self,
        username: &str,
        password: &str,
        realm: Option<&str>,
    ) -> Result<bool> {
        // Hash the password if a hashing algorithm is specified
        let hashed_password = self.hash_password(password);

        // Use raw SQL query to be flexible with table and column names
        let realm_column = match self.realm_column {
            Some(ref realm_col) => format!("AND {} = $3", realm_col),
            None => "".to_string(),
        };
        let query = format!(
            "SELECT COUNT(*) FROM {} WHERE {} = $1 AND {} = $2 {}",
            self.table_name, self.username_column, self.password_column, realm_column
        );

        let count: i64 = sqlx::query_scalar::<_, i64>(&query)
            .bind(username)
            .bind(&hashed_password)
            .fetch_one(&self.db)
            .await
            .map_err(|e| anyhow!("Database query error: {}", e))?;

        Ok(count > 0)
    }

    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let id_column = match self.id_column {
            Some(ref id_col) => format!("{},", id_col),
            None => "".to_string(),
        };
        let enabled_column = match self.enabled_column {
            Some(ref enabled_col) => format!(", {}", enabled_col),
            None => "".to_string(),
        };
        let query = format!(
            "SELECT {} {}, {} {} FROM {} WHERE {} = $1",
            id_column,
            self.username_column,
            self.password_column,
            enabled_column,
            self.table_name,
            self.username_column
        );

        let row = sqlx::query(&query)
            .bind(username)
            .fetch_one(&self.db)
            .await
            .map_err(|e| anyhow!("Database query error: {}", e))?;

        // Map the database row to a SipUser
        let id: i64 = row
            .try_get(
                self.id_column
                    .as_ref()
                    .unwrap_or(&"id".to_string())
                    .as_str(),
            )
            .unwrap_or(0);
        let db_username: String = row
            .try_get(self.username_column.as_str())
            .unwrap_or_default();
        let password: String = row
            .try_get(self.password_column.as_str())
            .unwrap_or_default();
        let enabled: bool = row
            .try_get(
                self.enabled_column
                    .as_ref()
                    .unwrap_or(&"enabled".to_string())
                    .as_str(),
            )
            .unwrap_or(true);

        Ok(SipUser {
            id: id as u64,
            username: db_username,
            password: Some(password),
            enabled,
            realm: realm.map(|r| r.to_string()),
        })
    }
}
