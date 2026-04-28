use crate::addons::{Addon, SidebarItem, export_reload::ExportReloadHandler};
use crate::app::AppState;
use async_trait::async_trait;
use axum::Router;
use serde_json::{json, Value as JsonValue};
use tower_http::services::ServeDir;

pub mod console;
pub mod models;
pub mod services;
mod tests;

pub struct QueueAddon;

impl Default for QueueAddon {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueAddon {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Addon for QueueAddon {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &'static str {
        "queue"
    }

    fn name(&self) -> &'static str {
        "Queue Manager"
    }

    fn description(&self) -> &'static str {
        "Manage call queues and agents"
    }

    async fn initialize(&self, _state: AppState) -> anyhow::Result<()> {
        // Migrations are handled globally for now, or we can run them here
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        let console_state = state.console.clone()?;
        let base_path = console_state.base_path().to_string();
        let router = console::handlers::urls().with_state(console_state);

        let static_fs_path = if std::path::Path::new("src/addons/queue/static").exists() {
            "src/addons/queue/static"
        } else {
            "static/queue"
        };
        let static_url_prefix = state.config().static_path();

        Some(
            Router::new()
                .nest_service(&format!("{}/queue", static_url_prefix), ServeDir::new(static_fs_path))
                .nest(&base_path, router),
        )
    }

    fn sidebar_items(&self, state: AppState) -> Vec<SidebarItem> {
        let base_path = state
            .console
            .as_ref()
            .map(|c| c.base_path().to_string())
            .unwrap_or_else(|| "/console".to_string());
        vec![SidebarItem {
            name: "Queues".to_string(),
            name_key: Some("queue.sidebar_name".to_string()),
            icon: r#"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-5"><path stroke-linecap="round" stroke-linejoin="round" d="M3.75 12h16.5m-16.5 3.75h16.5M3.75 19.5h16.5M5.625 4.5h12.75a1.875 1.875 0 0 1 0 3.75H5.625a1.875 1.875 0 0 1 0-3.75Z" /></svg>"#.to_string(),
            url: format!("{}/queues", base_path),
            permission: None,
        }]
    }

    fn inject_scripts(&self) -> Vec<crate::addons::ScriptInjection> {
        // Use a regex that matches any base path followed by /extensions/ or /routing/
        vec![crate::addons::ScriptInjection {
            url_path_regex: r"^/.+/extensions/.*|^/.+/extensions/new$|^/.+/routing/.*|^/.+/routing/new$",
            script_url: "/static/queue/queue_extension.js".to_string(),
        }]
    }

    fn locales_dir(&self) -> Option<String> {
        // Prefer the source-tree path during development; fall back to the
        // deployment path used inside the Docker image.
        let dev = "src/addons/queue/locales";
        let deployed = "locales/queue";
        if std::path::Path::new(dev).exists() {
            Some(dev.to_string())
        } else {
            Some(deployed.to_string())
        }
    }

    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![Box::new(models::Migration)]
    }

    fn export_reload_handler(&self) -> Option<Box<dyn ExportReloadHandler>> {
        Some(Box::new(QueueExportReloadHandler))
    }
}

// ── Export/Reload Handler ──────────────────────────────────────────────────────

struct QueueExportReloadHandler;

#[async_trait]
impl ExportReloadHandler for QueueExportReloadHandler {
    fn addon_id(&self) -> &str {
        "queue"
    }

    fn display_name(&self) -> &str {
        "Queues"
    }

    async fn export_and_reload(&self, app_state: &AppState) -> Result<JsonValue, String> {
        let db = app_state.db();
        let proxy_cfg = crate::config::ProxyConfig::default();
        let exporter = crate::addons::queue::services::exporter::QueueExporter::new(db.clone());

        exporter.export_all(&proxy_cfg).await.map_err(|e| format!("Export failed: {}", e))?;

        app_state
            .sip_server()
            .inner
            .data_context
            .reload_queues(true, None)
            .await
            .map_err(|e| format!("Reload failed: {}", e))?;

        if let Some(ref console) = app_state.console {
            console.clear_pending_reload();
        }

        Ok(json!({"status": "ok"}))
    }
}
