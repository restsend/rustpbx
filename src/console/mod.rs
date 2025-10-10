use crate::config::ConsoleConfig;
use crate::console::middleware::RenderTemplate;
use anyhow::{Context, Result};
use axum::response::{IntoResponse, Response};
use minijinja::{Environment, path_loader};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;
use tracing::debug;

pub mod auth;
mod handlers;
pub mod middleware;
pub mod migration;
pub mod models;
pub use handlers::router;

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
}

impl ConsoleState {
    pub async fn initialize(config: ConsoleConfig) -> Result<Arc<Self>> {
        let ConsoleConfig {
            database_url,
            session_secret,
            base_path,
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
        Ok(Arc::new(Self {
            db,
            session_key,
            base_path,
        }))
    }

    pub fn render(&self, template: &str, ctx: serde_json::Value) -> Response {
        let mut ctx = ctx;
        if ctx.is_object() {
            if let Some(map) = ctx.as_object_mut() {
                map.entry("base_path")
                    .or_insert_with(|| serde_json::Value::String(self.base_path().to_string()));
                map.entry("logout_url")
                    .or_insert_with(|| serde_json::Value::String(self.url_for("/logout")));
                map.entry("forgot_url")
                    .or_insert_with(|| serde_json::Value::String(self.forgot_url()));
                map.entry("login_url")
                    .or_insert_with(|| serde_json::Value::String(self.url_for("/login")));
                map.entry("register_url")
                    .or_insert_with(|| serde_json::Value::String(self.register_url(None)));
                map.entry("username").or_insert(serde_json::Value::Null);
                map.entry("email").or_insert(serde_json::Value::Null);
                map.entry("site_version").or_insert_with(|| {
                    serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string())
                });
                map.entry("site_name")
                    .or_insert_with(|| serde_json::Value::String("RustPBX".to_string()));
                map.entry("page_title")
                    .or_insert_with(|| serde_json::Value::String("RustPBX admin".to_string()));
                map.entry("site_description").or_insert_with(|| {
                    serde_json::Value::String("RustPBX - A Rust-based PBX system".to_string())
                });
                map.entry("site_url").or_insert_with(|| {
                    serde_json::Value::String("https://rustpbx.com".to_string())
                });
                map.entry("site_footer").or_insert_with(|| {
                    serde_json::Value::String("© 2025 RustPBX. All rights reserved.".to_string())
                });
                map.entry("site_logo").or_insert_with(|| {
                    serde_json::Value::String("/static/images/logo.png".to_string())
                });
                map.entry("site_logo_mini").or_insert_with(|| {
                    serde_json::Value::String("/static/images/logo-mini.png".to_string())
                });
                map.entry("favicon_url").or_insert_with(|| {
                    serde_json::Value::String("/static/images/favicon.png".to_string())
                });
            }
        }

        let start_time = std::time::Instant::now();
        let mut tmpl_env = Environment::new();
        tmpl_env.set_loader(path_loader("templates"));

        let r = RenderTemplate {
            tmpl_env: &tmpl_env,
            template_name: template,
            context: &serde_json::to_value(ctx).unwrap_or(serde_json::Value::Null),
        }
        .into_response();
        let elapsed = start_time.elapsed();
        debug!("rendered template '{}' in {:?}", template, elapsed);
        r
    }

    pub fn url_for(&self, suffix: &str) -> String {
        let trimmed = suffix.trim();
        if trimmed.is_empty() || trimmed == "/" {
            return format!("{}/", self.base_path);
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

    pub fn login_url(&self, next: Option<String>) -> String {
        let mut url = self.url_for("/login");
        if let Some(next) = next {
            url.push_str(&format!("?next={}", next));
        }
        url
    }

    pub fn register_url(&self, next: Option<String>) -> String {
        let mut url = self.url_for("/register");
        if let Some(next) = next {
            url.push_str(&format!("?next={}", next));
        }
        url
    }

    pub fn forgot_url(&self) -> String {
        self.url_for("/forgot")
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
