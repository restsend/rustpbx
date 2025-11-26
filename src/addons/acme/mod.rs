use async_trait::async_trait;
use crate::addons::{Addon, SidebarItem};
use crate::app::AppState;
use axum::{Router, routing::{get, post}, Extension};
use std::sync::{Arc, RwLock};
use std::collections::HashMap;

mod handlers;

#[derive(Clone)]
pub struct AcmeState {
    pub challenges: Arc<RwLock<HashMap<String, String>>>,
}

pub struct AcmeAddon {
    state: AcmeState,
}

impl AcmeAddon {
    pub fn new() -> Self { 
        Self {
            state: AcmeState {
                challenges: Arc::new(RwLock::new(HashMap::new())),
            }
        }
    }
}

#[async_trait]
impl Addon for AcmeAddon {
    fn id(&self) -> &'static str { "acme" }
    fn name(&self) -> &'static str { "SSL Certificates" }
    fn description(&self) -> &'static str { "Manage SSL certificates via Let's Encrypt" }

    async fn initialize(&self, _state: AppState) -> anyhow::Result<()> {
        // Initialize ACME background tasks or check config
        tracing::info!("ACME Addon initialized");
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        let r = Router::new()
            .route("/console/acme", get(handlers::ui_index))
            .route("/api/acme/request", post(handlers::request_cert))
            .route("/.well-known/acme-challenge/{token}", get(handlers::challenge))
            .with_state(state)
            .layer(Extension(self.state.clone()));
        Some(r)
    }

    fn template_dir(&self) -> Option<String> {
        Some("src/addons/acme/templates".to_string())
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

