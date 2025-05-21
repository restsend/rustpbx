use super::locator::{Location, Locator};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transport::SipAddr;
use sqlx::{pool::Pool, sqlite::Sqlite, Row};
use std::time::Instant;

/// Database backed Locator implementation
pub struct DbLocator {
    db: Pool<Sqlite>,
    table_name: String,
    id_column: String,
    username_column: String,
    expires_column: String,
    realm_column: String,
    destination_column: String,
    transport_column: String,
}

impl DbLocator {
    /// Create a new DbLocator with a database connection
    pub async fn new(
        url: String,
        table_name: Option<String>,
        id_column: Option<String>,
        username_column: Option<String>,
        expires_column: Option<String>,
        realm_column: Option<String>,
        destination_column: Option<String>,
        transport_column: Option<String>,
    ) -> Result<Self> {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .connect(&url)
            .await
            .map_err(|e| anyhow::anyhow!("Database connection error: {}", e))?;

        let db_locator = Self {
            db,
            table_name: table_name.unwrap_or_else(|| "locations".to_string()),
            id_column: id_column.unwrap_or_else(|| "identifier".to_string()),
            username_column: username_column.unwrap_or_else(|| "aor".to_string()),
            expires_column: expires_column.unwrap_or_else(|| "expires".to_string()),
            realm_column: realm_column.unwrap_or_else(|| "realm".to_string()),
            destination_column: destination_column.unwrap_or_else(|| "destination".to_string()),
            transport_column: transport_column.unwrap_or_else(|| "transport".to_string()),
        };

        // Ensure the table exists
        db_locator.ensure_table().await?;

        Ok(db_locator)
    }

    /// Ensure the locations table exists in the database
    async fn ensure_table(&self) -> Result<()> {
        // Check if table exists, if not create it
        let query = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id INTEGER PRIMARY KEY,
                {} TEXT NOT NULL UNIQUE,
                {} TEXT NOT NULL,
                {} INTEGER NOT NULL,
                {} TEXT NOT NULL,
                {} TEXT NOT NULL,
                {} TEXT NOT NULL,
                last_modified TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            self.table_name,
            self.id_column,
            self.username_column,
            self.expires_column,
            self.realm_column,
            self.destination_column,
            self.transport_column
        );

        sqlx::query(&query)
            .execute(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Error creating table: {}", e))?;

        Ok(())
    }
}

#[async_trait]
impl Locator for DbLocator {
    async fn register(
        &self,
        username: &str,
        realm: Option<&str>,
        location: Location,
    ) -> Result<()> {
        let identifier = self.get_identifier(username, realm);
        let aor = location.aor.to_string();
        let expires = location.expires;

        // Extract SipAddr components
        let (host, transport) = match &location.destination {
            SipAddr { r#type, addr } => {
                let transport = match r#type {
                    Some(t) => t.to_string(),
                    None => "UDP".to_string(), // Default to UDP if not specified
                };
                (addr, transport)
            }
        };

        // For SQLite, we can use INSERT OR REPLACE which is simpler than ON CONFLICT
        let query = format!(
            "INSERT OR REPLACE INTO {} ({}, {}, {}, {}, {}, {}, last_modified)
             VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            self.table_name,
            self.id_column,
            self.username_column,
            self.expires_column,
            self.realm_column,
            self.destination_column,
            self.transport_column
        );

        sqlx::query(&query)
            .bind(identifier)
            .bind(aor)
            .bind(expires as i64)
            .bind(realm)
            .bind(host.to_string())
            .bind(transport)
            .execute(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on register: {}", e))?;

        Ok(())
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm);

        let query = format!(
            "DELETE FROM {} WHERE {} = ?",
            self.table_name, self.id_column
        );

        sqlx::query(&query)
            .bind(identifier)
            .execute(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on unregister: {}", e))?;

        Ok(())
    }

    async fn lookup(&self, username: &str, realm: Option<&str>) -> Result<Vec<Location>> {
        let identifier = self.get_identifier(username, realm);

        let query = format!(
            "SELECT {}, {},  {}, {} FROM {} WHERE {} = ?",
            self.username_column,
            self.expires_column,
            self.destination_column,
            self.transport_column,
            self.table_name,
            self.id_column
        );

        let rows = sqlx::query(&query)
            .bind(identifier)
            .fetch_all(&self.db)
            .await
            .map_err(|e| anyhow::anyhow!("Database error on lookup: {}", e))?;

        if rows.is_empty() {
            return Err(anyhow::anyhow!("User not found"));
        }

        let mut locations = Vec::new();
        for row in rows {
            let aor_str: String = row
                .try_get(self.username_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting aor: {}", e))?;
            let expires: i64 = row
                .try_get(self.expires_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting expires: {}", e))?;
            let host: String = row
                .try_get(self.destination_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting destination host: {}", e))?;
            let transport_str: String = row
                .try_get(self.transport_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting destination transport: {}", e))?;

            // Parse the aor into a Uri
            let aor = rsip::Uri::try_from(aor_str.as_str())
                .map_err(|e| anyhow::anyhow!("Error parsing aor: {}", e))?;

            // Parse transport from string
            let transport = match transport_str.to_uppercase().as_str() {
                "UDP" => rsip::transport::Transport::Udp,
                "TCP" => rsip::transport::Transport::Tcp,
                "TLS" => rsip::transport::Transport::Tls,
                "WSS" => rsip::transport::Transport::Wss,
                _ => rsip::transport::Transport::Udp, // Default to UDP
            };

            // Create SipAddr
            let destination = SipAddr {
                r#type: Some(transport),
                addr: host.try_into()?,
            };

            locations.push(Location {
                aor,
                expires: expires as u32,
                destination,
                last_modified: Instant::now(), // We can't store Instant in DB, so create a new one
            });
        }

        Ok(locations)
    }
}
