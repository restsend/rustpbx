use crate::app::AppState;
use async_trait::async_trait;
use axum::Router;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarItem {
    pub name: String,
    pub icon: String, // FontAwesome class or SVG
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
}

#[async_trait]
pub trait Addon: Send + Sync {
    /// Unique identifier for the addon
    fn id(&self) -> &'static str;

    /// Display name of the addon
    fn name(&self) -> &'static str;

    /// Description of the addon
    fn description(&self) -> &'static str {
        ""
    }

    /// Initialize the addon (migrations, background tasks, etc.)
    async fn initialize(&self, state: AppState) -> anyhow::Result<()>;

    /// Return API and UI routes to be merged into the main application Router
    fn router(&self, state: AppState) -> Option<Router>;

    /// Return Sidebar menu items
    fn sidebar_items(&self) -> Vec<SidebarItem> {
        vec![]
    }

    /// Return the path to the addon's templates directory (relative to workspace root)
    fn template_dir(&self) -> Option<String> {
        None
    }

    /// Return Settings page injection items (HTML fragments or config definitions)
    fn settings_items(&self) -> Option<String> {
        None
    }
}

pub mod acme;
pub mod registry;
