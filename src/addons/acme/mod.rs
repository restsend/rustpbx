use crate::addons::{Addon, SidebarItem};
use crate::app::AppState;
use async_trait::async_trait;
use axum::{
    Extension, Router,
    routing::{get, post},
};
use dashmap::DashMap;
use serde::Serialize;
use std::sync::{Arc, RwLock};
use tokio::sync::RwLock as TokioRwLock;

pub mod config;
pub use config::AcmeConfig;

mod handlers;

#[derive(Clone, Debug, Serialize)]
pub enum AcmeStatus {
    None,
    Running(String),
    Success(String),
    Error(String),
}

#[derive(Clone)]
pub struct AcmeState {
    pub challenges: Arc<DashMap<String, String>>,
    pub status: Arc<RwLock<AcmeStatus>>,
    /// Auto-renewal configuration (loaded from config.toml)
    pub auto_renew_config: Arc<TokioRwLock<AcmeConfig>>,
}

pub struct AcmeAddon {
    state: AcmeState,
}

impl Default for AcmeAddon {
    fn default() -> Self {
        Self::new()
    }
}

impl AcmeAddon {
    pub fn new() -> Self {
        Self {
            state: AcmeState {
                challenges: Arc::new(DashMap::new()),
                status: Arc::new(RwLock::new(AcmeStatus::None)),
                auto_renew_config: Arc::new(TokioRwLock::new(AcmeConfig::default())),
            },
        }
    }

    /// Load auto-renew configuration from addon-specific config file.
    /// Tries `<config_dir>/acme.toml` first, falls back to defaults.
    pub async fn load_config(&self, config_path: &Option<String>) {
        let addon_config_path = acme_config_path(config_path);
        let mut loaded = false;
        if addon_config_path.exists() {
            match tokio::fs::read_to_string(&addon_config_path).await {
                Ok(content) => match toml::from_str::<AcmeConfig>(&content) {
                    Ok(acme_config) => {
                        let mut auto_renew = self.state.auto_renew_config.write().await;
                        *auto_renew = acme_config;
                        loaded = true;
                        tracing::info!("ACME config loaded from {}", addon_config_path.display());
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse acme.toml: {}", e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read acme.toml: {}", e);
                }
            }
        }
        if !loaded {
            tracing::info!("ACME using default configuration (no acme.toml found)");
        }
    }
}

/// `acme.toml` next to the main config file; the console saves the
/// auto-renew settings there and startup reads them back.
pub(crate) fn acme_config_path(config_path: &Option<String>) -> std::path::PathBuf {
    config_path
        .as_deref()
        .and_then(|path| std::path::Path::new(path).parent())
        .map(|dir| dir.join("acme.toml"))
        .unwrap_or_else(|| std::path::PathBuf::from("acme.toml"))
}

/// Persist the auto-renew settings to `path`.
pub(crate) async fn save_acme_config(
    path: &std::path::Path,
    config: &AcmeConfig,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, toml::to_string_pretty(config)?).await?;
    Ok(())
}

#[async_trait]
impl Addon for AcmeAddon {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &'static str {
        "acme"
    }
    fn name(&self) -> &'static str {
        "SSL Certificates"
    }
    fn description(&self) -> &'static str {
        "Manage SSL certificates via Let's Encrypt"
    }
    fn screenshots(&self) -> Vec<&'static str> {
        vec!["/static/acme/screenshot.png"]
    }
    async fn initialize(&self, state: AppState) -> anyhow::Result<()> {
        // Load auto-renew configuration from addon-specific config file
        self.load_config(&state.config_path).await;

        // Spawn background task for certificate expiry checking
        let state_clone = self.state.clone();
        let app_state = state.clone();
        let cancel_token = state.token().child_token();
        crate::utils::spawn(async move {
            handlers::spawn_auto_renew_checker(state_clone, app_state, cancel_token).await;
        });

        tracing::info!("ACME Addon initialized");
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        if state.config().demo_mode {
            return None;
        }

        let static_fs_path = if std::path::Path::new("src/addons/acme/static").exists() {
            "src/addons/acme/static"
        } else if std::path::Path::new("static/acme").exists() {
            "static/acme"
        } else {
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/addons/acme/static")
        };
        let static_url_prefix = state.config().static_path();

        // Get base_path and api_prefix from console config if available
        let (base_path, api_prefix) = state
            .console
            .as_ref()
            .map(|c| (c.base_path().to_string(), c.api_prefix().to_string()))
            .unwrap_or_else(|| ("/console".to_string(), "/api".to_string()));

        let mut protected = Router::new()
            .nest_service(
                &format!("{}/acme", static_url_prefix),
                tower_http::services::ServeDir::new(static_fs_path),
            )
            .route(&format!("{base_path}/acme"), get(handlers::ui_index))
            .route(
                &format!("{api_prefix}/acme/request"),
                post(handlers::request_cert),
            )
            .route(&format!("{api_prefix}/acme/status"), get(handlers::status))
            .route(
                &format!("{api_prefix}/acme/auto-renew"),
                get(handlers::get_auto_renew_config),
            )
            .route(
                &format!("{api_prefix}/acme/auto-renew"),
                post(handlers::set_auto_renew_config),
            );

        #[cfg(feature = "console")]
        if let Some(console_state) = state.console.clone() {
            protected = protected.route_layer(axum::middleware::from_extractor_with_state::<
                crate::console::middleware::AuthRequired,
                std::sync::Arc<crate::console::ConsoleState>,
            >(console_state));
        }

        let public = Router::new().route(
            "/.well-known/acme-challenge/{token}",
            get(handlers::challenge),
        );

        let r = Router::new()
            .merge(protected)
            .merge(public)
            .with_state(state)
            .layer(Extension(self.state.clone()));
        Some(r)
    }

    fn locales_dir(&self) -> Option<String> {
        Some(crate::addons::addon_locales_dir("acme"))
    }

    fn sidebar_items(&self, state: AppState) -> Vec<SidebarItem> {
        if state.config().demo_mode {
            return vec![];
        }
        let base_path = state
            .console
            .as_ref()
            .map(|c| c.base_path().to_string())
            .unwrap_or_else(|| "/console".to_string());
        vec![SidebarItem {
            name: "SSL Certificates".to_string(),
            name_key: Some("acme.sidebar_name".to_string()),
            icon: r#"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-5"><path stroke-linecap="round" stroke-linejoin="round" d="M9 12.75 11.25 15 15 9.75M21 12c0 1.268-.63 2.39-1.593 3.068a3.745 3.745 0 0 1-1.043 3.296 3.745 3.745 0 0 1-3.296 1.043A3.745 3.745 0 0 1 12 21c-1.268 0-2.39-.63-3.068-1.593a3.746 3.746 0 0 1-3.296-1.043 3.745 3.745 0 0 1-1.043-3.296A3.745 3.745 0 0 1 3 12c0-1.268.63-2.39 1.593-3.068a3.745 3.745 0 0 1 1.043-3.296 3.746 3.746 0 0 1 3.296-1.043A3.746 3.746 0 0 1 12 3c1.268 0 2.39.63 3.068 1.593a3.746 3.746 0 0 1 3.296 1.043 3.746 3.746 0 0 1 1.043 3.296A3.745 3.745 0 0 1 21 12Z" /></svg>"#.to_string(),
            url: format!("{}/acme", base_path),
            permission: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acme_config_path_uses_main_config_directory() {
        assert_eq!(
            acme_config_path(&Some("config/rustpbx.toml".to_string())),
            std::path::PathBuf::from("config/acme.toml")
        );
        assert_eq!(acme_config_path(&None), std::path::PathBuf::from("acme.toml"));
    }

    #[tokio::test]
    async fn saved_auto_renew_settings_load_back() {
        let dir = tempfile::tempdir().unwrap();
        let main_config = dir.path().join("config.toml").to_string_lossy().to_string();
        let saved = AcmeConfig {
            auto_renew: true,
            renewal_threshold_hours: 48,
            renew_https: true,
            renew_sips: false,
            domain: Some("pbx.example.com".to_string()),
        };
        save_acme_config(&acme_config_path(&Some(main_config.clone())), &saved)
            .await
            .unwrap();

        let addon = AcmeAddon::new();
        addon.load_config(&Some(main_config)).await;
        let loaded = addon.state.auto_renew_config.read().await.clone();
        assert!(loaded.auto_renew);
        assert_eq!(loaded.renewal_threshold_hours, 48);
        assert!(loaded.renew_https);
        assert_eq!(loaded.domain.as_deref(), Some("pbx.example.com"));
    }
}
