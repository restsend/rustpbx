use crate::addons::{Addon, SidebarItem};
use crate::app::AppState;
use async_trait::async_trait;
use axum::Router;
use tower_http::services::ServeDir;

pub mod console;
pub mod models;
pub mod services;
mod tests;

pub struct QueueAddon;

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

        let static_path = if std::path::Path::new("src/addons/queue/static").exists() {
            "src/addons/queue/static"
        } else {
            "static/queue"
        };

        Some(
            Router::new()
                .nest_service("/static/queue", ServeDir::new(static_path))
                .nest(&base_path, router),
        )
    }

    fn sidebar_items(&self, _state: AppState) -> Vec<SidebarItem> {
        vec![SidebarItem {
            name: "Queues".to_string(),
            icon: r#"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-5"><path stroke-linecap="round" stroke-linejoin="round" d="M3.75 12h16.5m-16.5 3.75h16.5M3.75 19.5h16.5M5.625 4.5h12.75a1.875 1.875 0 0 1 0 3.75H5.625a1.875 1.875 0 0 1 0-3.75Z" /></svg>"#.to_string(),
            url: "/console/queues".to_string(),
            permission: None,
        }]
    }

    fn inject_scripts(&self) -> Vec<crate::addons::ScriptInjection> {
        vec![crate::addons::ScriptInjection {
            url_path_regex: r"^/console/extensions/.*|/console/extensions/new$|^/console/routing/.*|/console/routing/new$",
            script_url: "/static/queue/queue_extension.js".to_string(),
        }]
    }
}
