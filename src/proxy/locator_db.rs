use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transport::SipAddr;
use sqlx::{pool::Pool, sqlite::Sqlite, Row};
use std::time::Instant;

use super::locator::{Location, Locator};

/// Database backed Locator implementation
pub struct DbLocator {
    db: Pool<Sqlite>,
    table_name: String,
    identifier_column: String,
    aor_column: String,
    expires_column: String,
    destination_host_column: String,
    destination_port_column: String,
    destination_transport_column: String,
}

impl DbLocator {
    /// Create a new DbLocator with a database connection
    pub async fn new(
        url: String,
        table_name: Option<String>,
        identifier_column: Option<String>,
        aor_column: Option<String>,
        expires_column: Option<String>,
        destination_host_column: Option<String>,
        destination_port_column: Option<String>,
        destination_transport_column: Option<String>,
    ) -> Result<Self> {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .connect(&url)
            .await
            .map_err(|e| anyhow::anyhow!("Database connection error: {}", e))?;

        let db_locator = Self {
            db,
            table_name: table_name.unwrap_or_else(|| "locations".to_string()),
            identifier_column: identifier_column.unwrap_or_else(|| "identifier".to_string()),
            aor_column: aor_column.unwrap_or_else(|| "aor".to_string()),
            expires_column: expires_column.unwrap_or_else(|| "expires".to_string()),
            destination_host_column: destination_host_column
                .unwrap_or_else(|| "destination_host".to_string()),
            destination_port_column: destination_port_column
                .unwrap_or_else(|| "destination_port".to_string()),
            destination_transport_column: destination_transport_column
                .unwrap_or_else(|| "destination_transport".to_string()),
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
                {} INTEGER NOT NULL,
                {} TEXT NOT NULL,
                last_modified TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
            self.table_name,
            self.identifier_column,
            self.aor_column,
            self.expires_column,
            self.destination_host_column,
            self.destination_port_column,
            self.destination_transport_column
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
        let (host, port, transport) = match &location.destination {
            SipAddr { r#type, addr } => {
                let host = match &addr.host {
                    rsip::host_with_port::Host::Domain(domain) => domain.to_string(),
                    rsip::host_with_port::Host::IpAddr(ip) => ip.to_string(),
                };

                let port = addr.port.as_ref().map_or(5060, |p| p.value().to_owned());

                let transport = match r#type {
                    Some(t) => t.to_string(),
                    None => "UDP".to_string(), // Default to UDP if not specified
                };

                (host, port, transport)
            }
        };

        // For SQLite, we can use INSERT OR REPLACE which is simpler than ON CONFLICT
        let query = format!(
            "INSERT OR REPLACE INTO {} ({}, {}, {}, {}, {}, {}, last_modified)
             VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            self.table_name,
            self.identifier_column,
            self.aor_column,
            self.expires_column,
            self.destination_host_column,
            self.destination_port_column,
            self.destination_transport_column
        );

        sqlx::query(&query)
            .bind(identifier)
            .bind(aor)
            .bind(expires as i64)
            .bind(host)
            .bind(port as i64)
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
            self.table_name, self.identifier_column
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
            "SELECT {}, {}, {}, {}, {} FROM {} WHERE {} = ?",
            self.aor_column,
            self.expires_column,
            self.destination_host_column,
            self.destination_port_column,
            self.destination_transport_column,
            self.table_name,
            self.identifier_column
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
                .try_get(self.aor_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting aor: {}", e))?;
            let expires: i64 = row
                .try_get(self.expires_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting expires: {}", e))?;
            let host: String = row
                .try_get(self.destination_host_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting destination host: {}", e))?;
            let port: i64 = row
                .try_get(self.destination_port_column.as_str())
                .map_err(|e| anyhow::anyhow!("Error getting destination port: {}", e))?;
            let transport_str: String = row
                .try_get(self.destination_transport_column.as_str())
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

            // Create a HostWithPort from the host and port
            let host_with_port = match host.parse::<std::net::IpAddr>() {
                Ok(ip_addr) => {
                    // It's an IP address
                    rsip::HostWithPort {
                        host: ip_addr.into(),
                        port: Some((port as u16).into()),
                    }
                }
                Err(_) => {
                    // It's a domain name
                    rsip::HostWithPort {
                        host: host
                            .parse()
                            .map_err(|e| anyhow::anyhow!("Error parsing host: {}", e))?,
                        port: Some((port as u16).into()),
                    }
                }
            };

            // Create SipAddr
            let destination = SipAddr {
                r#type: Some(transport),
                addr: host_with_port,
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
