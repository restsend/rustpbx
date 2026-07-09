use crate::app::AppState;
use async_trait::async_trait;
use axum::Router;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[cfg(feature = "console")]
use crate::console::ConsoleState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarItem {
    pub name: String,
    /// i18n key for the sidebar item name (e.g., "queue.sidebar_name")
    /// If set, the template will use this for translation lookup.
    pub name_key: Option<String>,
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
    #[cfg(feature = "commerce")]
    pub license_status: Option<String>,
    #[cfg(feature = "commerce")]
    pub license_expiry: Option<String>,
    #[cfg(feature = "commerce")]
    pub license_plan: Option<String>,
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

    /// Return the path to this addon's locale directory, if any.
    ///
    /// Translation files in this directory (e.g. `en.toml`, `zh.toml`) will
    /// be merged into the global i18n cache under the addon's own namespace.
    /// Conventionally the keys should be prefixed with the addon id, e.g.:
    ///
    /// ```toml
    /// # src/addons/queue/locales/en.toml
    /// [queue]
    /// title = "Call Queues"
    /// ```
    fn locales_dir(&self) -> Option<String> {
        None
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

    /// Return a dialplan inspector for intercepting SIP INVITEs
    fn dialplan_inspector(&self) -> Option<Box<dyn crate::proxy::call::DialplanInspector>> {
        None
    }

    fn queue_location_enricher(
        &self,
    ) -> Option<Arc<dyn crate::proxy::call::QueueLocationEnricher>> {
        None
    }

    /// Return database migrations for this addon.
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![]
    }

    /// Seed fixtures for the addon
    async fn seed_fixtures(&self, _state: AppState) -> anyhow::Result<()> {
        Ok(())
    }

    /// Authenticate a user
    async fn authenticate(
        &self,
        _state: AppState,
        _identifier: &str,
        _password: &str,
    ) -> anyhow::Result<Option<crate::models::user::Model>> {
        Ok(None)
    }

    /// Return an export/reload handler for this addon.
    /// Addons that support export-and-reload (e.g. queues, IVR, CC) should
    /// return a handler here so the cluster reload UI can discover them.
    fn export_reload_handler(&self) -> Option<Box<dyn export_reload::ExportReloadHandler>> {
        None
    }

    // ── Console page / API route hooks ──────────────────────────────────────
    //
    // Addons can contribute page routes (rendered inside the console UI) and
    // API routes (served under the api prefix) by overriding these methods.
    // The routes are collected by AddonRegistry and merged at runtime, so the
    // core console code does not need compile-time feature gates.

    /// Return page routes to be merged into the console page router.
    /// These routes are rendered inside the console UI with session auth & CSRF.
    #[cfg(feature = "console")]
    fn console_page_routes(&self, _state: &ConsoleState) -> Option<Router<Arc<ConsoleState>>> {
        None
    }

    /// Return API routes to be merged into the console API router.
    /// These routes are served under the api prefix with api_auth_middleware.
    /// Addons should apply their own middleware layers as needed.
    #[cfg(feature = "console")]
    fn console_api_routes(&self, _state: &ConsoleState) -> Option<Router<Arc<ConsoleState>>> {
        None
    }

    /// Return a phone auth token validator for the API auth middleware.
    /// Only the first non-None result across all addons is used.
    #[cfg(feature = "console")]
    fn phone_auth_validator(&self, _state: &ConsoleState) -> Option<crate::auth::DynTokenValidator> {
        None
    }

    // ── Extension lifecycle hooks ────────────────────────────────────────────

    /// Called after an extension is created in the database.
    async fn on_extension_created(
        &self,
        _db: &sea_orm::DatabaseConnection,
        _extension: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called after an extension is updated in the database.
    async fn on_extension_updated(
        &self,
        _db: &sea_orm::DatabaseConnection,
        _extension: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called before an extension is deleted from the database.
    /// Implementations should clean up related resources.
    /// Errors are logged but do not block the deletion.
    async fn on_extension_deleting(
        &self,
        _db: &sea_orm::DatabaseConnection,
        _extension: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

pub mod export_reload;
pub mod registry;

#[cfg(feature = "addon-observability")]
pub mod observability;

#[cfg(feature = "addon-acme")]
pub mod acme;
#[cfg(feature = "addon-archive")]
pub mod archive;
#[cfg(feature = "addon-transcript")]
pub mod transcript;
#[cfg(feature = "addon-wholesale")]
pub mod wholesale;

#[cfg(feature = "addon-cc")]
pub mod cc;
pub mod queue;
#[cfg(feature = "addon-sbc")]
pub mod sbc;

#[cfg(feature = "addon-endpoint-manager")]
pub mod endpoint_manager;

#[cfg(feature = "addon-enterprise-auth")]
pub mod enterprise_auth;

#[cfg(feature = "addon-voicemail")]
pub mod voicemail;

#[cfg(feature = "addon-ivr-editor")]
pub mod ivr_editor;

#[cfg(feature = "addon-telemetry")]
pub mod telemetry;
