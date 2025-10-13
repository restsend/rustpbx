use crate::config::ConsoleConfig;
use crate::console::middleware::RenderTemplate;
use anyhow::Result;
use axum::response::{IntoResponse, Response};
use minijinja::{Environment, path_loader};
use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::debug;

pub mod auth;
mod handlers;
pub mod middleware;
pub use handlers::router;

#[derive(Clone)]
pub struct ConsoleState {
    db: DatabaseConnection,
    session_key: Arc<Vec<u8>>,
    base_path: String,
}

impl ConsoleState {
    pub async fn initialize(db: DatabaseConnection, config: ConsoleConfig) -> Result<Arc<Self>> {
        let ConsoleConfig {
            session_secret,
            base_path,
        } = config;

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
            context: &ctx,
        }
        .into_response();
        let elapsed = start_time.elapsed();
        debug!("rendered template '{}' in {:?}", template, elapsed);
        r
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
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
