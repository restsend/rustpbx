use crate::addons::{Addon, ScriptInjection, SidebarItem};
use crate::app::AppState;
use async_trait::async_trait;
use axum::{Router, routing::get};

pub mod handlers;
pub mod models;

pub struct TranscriptAddon;

impl TranscriptAddon {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Addon for TranscriptAddon {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &'static str {
        "transcript"
    }

    fn name(&self) -> &'static str {
        "Call Transcription"
    }

    fn description(&self) -> &'static str {
        "Transcribe call recordings using SenseVoice, locally hosted speech recognition supporting multiple languages."
    }

    fn screenshots(&self) -> Vec<&'static str> {
        vec![
            "/static/transcript/transcript_callrecord.png",
            "/static/transcript/transcript_download.png",
        ]
    }

    async fn initialize(&self, _state: AppState) -> anyhow::Result<()> {
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        if let Some(console) = &state.console {
            let base = console.base_path();
            let static_path = if std::path::Path::new("src/addons/transcript/static").exists() {
                "src/addons/transcript/static"
            } else {
                "static/transcript"
            };

            let router = Router::new().nest_service(
                "/static/transcript",
                tower_http::services::ServeDir::new(static_path),
            );

            let router = router
                .route(
                    &format!("{}{}", base, "/call-records/{id}/transcript"),
                    get(handlers::get_call_record_transcript)
                        .post(handlers::trigger_call_record_transcript),
                )
                .route(
                    &format!("{}/transcript", base),
                    get(handlers::get_settings).post(handlers::update_settings),
                )
                .with_state(console.clone());
            Some(router)
        } else {
            None
        }
    }

    fn sidebar_items(&self, _state: AppState) -> Vec<SidebarItem> {
        vec![SidebarItem {
            name: "Call Transcription".to_string(),
            icon: r#"<svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" stroke-width="1.5" stroke="currentColor" class="size-5"><path stroke-linecap="round" stroke-linejoin="round" d="M12 18.75a6 6 0 0 0 6-6v-1.5m-6 7.5a6 6 0 0 1-6-6v-1.5m6 7.5v3.75m-3.75 0h7.5M12 15.75a3 3 0 0 1-3-3V4.5a3 3 0 1 1 6 0v8.25a3 3 0 0 1-3 3Z" /></svg>"#.to_string(),
            url: "/console/transcript".to_string(),
            permission: None,
        }]
    }

    fn inject_scripts(&self) -> Vec<ScriptInjection> {
        vec![ScriptInjection {
            url_path_regex: r"^/console/call-records/\d+$",
            script_url: "/static/transcript/transcript_addon.js".to_string(),
        }]
    }
}
