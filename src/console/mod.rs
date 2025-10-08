use crate::config::ConsoleConfig;
use anyhow::{Context, Result};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;

pub mod auth;
pub mod handlers;
pub mod migration;
pub mod models;

fn prepare_sqlite_database(database_url: &str) -> Result<()> {
    let Some(path_part) = database_url.strip_prefix("sqlite://") else {
        return Ok(());
    };

    let (path_str, _) = path_part.split_once('?').unwrap_or((path_part, ""));
    if path_str.is_empty() || path_str.starts_with(':') {
        return Ok(());
    }

    let path = Path::new(path_str);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create directory for console database at {}",
                    parent.display()
                )
            })?;
        }
    }

    if !path.exists() {
        OpenOptions::new()
            .create(true)
            .write(true)
            .open(path)
            .with_context(|| {
                format!(
                    "failed to create console database file at {}",
                    path.display()
                )
            })?;
    }

    Ok(())
}

#[derive(Clone)]
pub struct ConsoleState {
    db: DatabaseConnection,
    session_key: Arc<Vec<u8>>,
    base_path: String,
    invite_code: Option<String>,
}

impl ConsoleState {
    pub async fn initialize(config: ConsoleConfig) -> Result<Arc<Self>> {
        let ConsoleConfig {
            database_url,
            session_secret,
            base_path,
            invite_code,
        } = config;

        prepare_sqlite_database(&database_url)?;

        let db = Database::connect(&database_url)
            .await
            .with_context(|| format!("failed to connect admin database: {}", database_url))?;

        migration::Migrator::up(&db, None)
            .await
            .context("failed to run console migrations")?;

        let key_material: [u8; 32] = Sha256::digest(session_secret.as_bytes()).into();
        let session_key = Arc::new(key_material.to_vec());

        let base_path = normalize_base_path(&base_path);
        let invite_code = invite_code
            .map(|code| code.trim().to_string())
            .filter(|c| !c.is_empty());

        Ok(Arc::new(Self {
            db,
            session_key,
            base_path,
            invite_code,
        }))
    }

    pub fn url_for(&self, suffix: &str) -> String {
        let trimmed = suffix.trim();
        if trimmed.is_empty() || trimmed == "/" {
            return self.base_path.clone();
        }
        if trimmed.starts_with('/') {
            if self.base_path == "/" {
                trimmed.to_string()
            } else {
                format!("{}{}", self.base_path, trimmed)
            }
        } else if self.base_path == "/" {
            format!("/{}", trimmed)
        } else {
            format!("{}/{}", self.base_path, trimmed)
        }
    }

    pub fn base_path(&self) -> &str {
        &self.base_path
    }

    pub fn invite_code(&self) -> Option<&str> {
        self.invite_code.as_deref()
    }
}

fn normalize_base_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/console".to_string();
    }
    let mut normalized = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    };
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.is_empty() {
        "/console".to_string()
    } else {
        normalized
    }
}
