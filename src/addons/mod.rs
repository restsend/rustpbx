use crate::app::AppState;
use async_trait::async_trait;
use axum::Router;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarItem {
    pub name: String,
    pub icon: String, // SVG content
    pub url: String,
    pub permission: Option<String>, // Permission required
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddonInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub config_url: Option<String>,
    pub category: AddonCategory,
    pub bundle: Option<String>,
    pub developer: String,
    pub website: String,
    pub cost: String,
    pub screenshots: Vec<String>,
    pub restart_required: bool,
    pub license_status: Option<String>,
    pub license_expiry: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AddonCategory {
    Community,
    Commercial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptInjection {
    pub url_path_regex: &'static str,
    pub script_url: String,
}

#[async_trait]
pub trait Addon: Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;

    /// Unique identifier for the addon
    fn id(&self) -> &'static str;

    /// Display name of the addon
    fn name(&self) -> &'static str;

    /// Description of the addon
    fn description(&self) -> &'static str {
        ""
    }

    fn category(&self) -> AddonCategory {
        AddonCategory::Community
    }

    fn bundle(&self) -> Option<&'static str> {
        None
    }

    fn developer(&self) -> &'static str {
        "miuda.ai"
    }

    fn website(&self) -> &'static str {
        ""
    }

    fn cost(&self) -> &'static str {
        "Free"
    }

    fn screenshots(&self) -> Vec<&'static str> {
        vec![]
    }

    /// Initialize the addon (migrations, background tasks, etc.)
    async fn initialize(&self, state: AppState) -> anyhow::Result<()>;

    /// Return API and UI routes to be merged into the main application Router
    fn router(&self, state: AppState) -> Option<Router>;

    /// Return Sidebar menu items
    fn sidebar_items(&self, _state: AppState) -> Vec<SidebarItem> {
        vec![]
    }

    /// Return the configuration URL for the addon
    fn config_url(&self, state: AppState) -> Option<String> {
        self.sidebar_items(state).first().map(|s| s.url.clone())
    }

    /// Return Settings page injection items (HTML fragments or config definitions)
    fn settings_items(&self) -> Option<String> {
        None
    }

    /// Return scripts to be injected into specific pages
    fn inject_scripts(&self) -> Vec<ScriptInjection> {
        vec![]
    }

    /// Return a hook for call record processing
    fn call_record_hook(
        &self,
        _db: &sea_orm::DatabaseConnection,
    ) -> Option<Box<dyn crate::callrecord::CallRecordHook>> {
        None
    }

    /// Return a hook for proxy server builder
    fn proxy_server_hook(
        &self,
        builder: crate::proxy::server::SipServerBuilder,
        _ctx: Arc<crate::app::CoreContext>,
    ) -> crate::proxy::server::SipServerBuilder {
        builder
    }

    /// Seed fixtures for the addon
    async fn seed_fixtures(&self, _state: AppState) -> anyhow::Result<()> {
        Ok(())
    }
}

pub mod registry;

#[cfg(feature = "addon-acme")]
pub mod acme;
#[cfg(feature = "addon-archive")]
pub mod archive;
#[cfg(feature = "addon-transcript")]
pub mod transcript;
#[cfg(feature = "addon-wholesale")]
pub mod wholesale;

pub mod queue;
