use crate::addons::{Addon, SidebarItem};
use crate::app::AppState;
use async_trait::async_trait;
use axum::{
    Extension, Router,
    routing::{get, post},
};
use serde::Serialize;
use std::collections::HashMap;
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
    pub challenges: Arc<RwLock<HashMap<String, String>>>,
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
                challenges: Arc::new(RwLock::new(HashMap::new())),
                status: Arc::new(RwLock::new(AcmeStatus::None)),
                auto_renew_config: Arc::new(TokioRwLock::new(AcmeConfig::default())),
            },
        }
    }

    /// Load auto-renew configuration from addon-specific config file.
    /// Tries `<config_dir>/acme.toml` first, falls back to defaults.
    pub async fn load_config(&self, config_path: &Option<String>) {
        let mut loaded = false;
        if let Some(path) = config_path {
            let config_dir = std::path::Path::new(path).parent();
            if let Some(dir) = config_dir {
                let addon_config_path = dir.join("acme.toml");
                if addon_config_path.exists() {
                    match tokio::fs::read_to_string(&addon_config_path).await {
                        Ok(content) => {
                            match toml::from_str::<AcmeConfig>(&content) {
                                Ok(acme_config) => {
                                    let mut auto_renew = self.state.auto_renew_config.write().await;
                                    *auto_renew = acme_config;
                                    loaded = true;
                                    tracing::info!(
                                        "ACME config loaded from {}",
                                        addon_config_path.display()
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse acme.toml: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to read acme.toml: {}", e);
                        }
                    }
                }
            }
        }
        if !loaded {
            tracing::info!("ACME using default configuration (no acme.toml found)");
        }
    }
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
        crate::utils::spawn(async move {
            handlers::spawn_auto_renew_checker(state_clone, app_state).await;
        });

        tracing::info!("ACME Addon initialized");
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        if state.config().demo_mode {
            return None;
        }

        let static_path = if std::path::Path::new("src/addons/acme/static").exists() {
            "src/addons/acme/static"
        } else {
            "static/acme"
        };

        // Get base_path and api_prefix from console config if available
        let (base_path, api_prefix) = state
            .console
            .as_ref()
            .map(|c| (c.base_path().to_string(), c.api_prefix().to_string()))
            .unwrap_or_else(|| ("/console".to_string(), "/api".to_string()));

        let mut protected = Router::new()
            .nest_service(
                "/static/acme",
                tower_http::services::ServeDir::new(static_path),
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
        let dev = "src/addons/acme/locales";
        let deployed = "locales/acme";
        if std::path::Path::new(dev).exists() {
            Some(dev.to_string())
        } else {
            Some(deployed.to_string())
        }
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
