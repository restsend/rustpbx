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
    fn id(&self) -> &'static str {
        "acme"
    }
    fn name(&self) -> &'static str {
        "SSL Certificates"
    }
    fn description(&self) -> &'static str {
        "Manage SSL certificates via Let's Encrypt"
    }

    async fn initialize(&self, _state: AppState) -> anyhow::Result<()> {
        // Initialize ACME background tasks or check config
        tracing::info!("ACME Addon initialized");
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        let mut protected = Router::new()
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

    fn sidebar_items(&self) -> Vec<SidebarItem> {
        vec![SidebarItem {
            name: "SSL Certificates".to_string(),
            icon: "mdi mdi-certificate".to_string(), // Assuming MDI icons are available
            url: "/console/acme".to_string(),
            permission: None,
        }]
    }
}
