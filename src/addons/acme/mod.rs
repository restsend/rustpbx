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
}

pub struct AcmeAddon {
    state: AcmeState,
}

impl AcmeAddon {
    pub fn new() -> Self {
        Self {
            state: AcmeState {
                challenges: Arc::new(RwLock::new(HashMap::new())),
                status: Arc::new(RwLock::new(AcmeStatus::None)),
            },
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
    async fn initialize(&self, _state: AppState) -> anyhow::Result<()> {
        // Initialize ACME background tasks or check config
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

        let mut protected = Router::new()
            .nest_service(
                "/static/acme",
                tower_http::services::ServeDir::new(static_path),
            )
            .route("/console/acme", get(handlers::ui_index))
            .route("/api/acme/request", post(handlers::request_cert))
            .route("/api/acme/status", get(handlers::status));

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

    fn sidebar_items(&self, state: AppState) -> Vec<SidebarItem> {
        if state.config().demo_mode {
            return vec![];
        }
        vec![SidebarItem {
            name: "SSL Certificates".to_string(),
            icon: r#"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-5"><path stroke-linecap="round" stroke-linejoin="round" d="M9 12.75 11.25 15 15 9.75M21 12c0 1.268-.63 2.39-1.593 3.068a3.745 3.745 0 0 1-1.043 3.296 3.745 3.745 0 0 1-3.296 1.043A3.745 3.745 0 0 1 12 21c-1.268 0-2.39-.63-3.068-1.593a3.746 3.746 0 0 1-3.296-1.043 3.745 3.745 0 0 1-1.043-3.296A3.745 3.745 0 0 1 3 12c0-1.268.63-2.39 1.593-3.068a3.745 3.745 0 0 1 1.043-3.296 3.746 3.746 0 0 1 3.296-1.043A3.746 3.746 0 0 1 12 3c1.268 0 2.39.63 3.068 1.593a3.746 3.746 0 0 1 3.296 1.043 3.746 3.746 0 0 1 1.043 3.296A3.745 3.745 0 0 1 21 12Z" /></svg>"#.to_string(),
            url: "/console/acme".to_string(),
            permission: None,
        }]
    }
}
