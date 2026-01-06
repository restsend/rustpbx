use crate::app::AppState;
use axum::{
    extract::{Json, State},
    response::IntoResponse,
    Extension,
};
use super::ArchiveState;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, value};
use tracing::{info, error};

#[derive(Serialize)]
pub struct ArchiveFile {
    name: String,
    size: u64,
    date: String,
}

#[derive(Deserialize)]
pub struct DeleteArchivePayload {
    filename: String,
}

#[derive(Deserialize)]
pub struct UpdateConfigPayload {
    enabled: bool,
    archive_time: String,
    timezone: String,
    retention_days: u32,
}

pub async fn ui_index(
    State(state): State<AppState>,
    Extension(archive_state): Extension<ArchiveState>,
) -> impl IntoResponse {
    #[cfg(feature = "console")]
    {
        if let Some(console) = &state.console {
            let archives = list_archive_files(&state.core.storage).await.unwrap_or_default();
            let config = archive_state.config.read().unwrap().clone();
            return console.render(
                "archive/archive_index.html",
                serde_json::json!({
                    "archives": archives,
                    "config": config,
                    "nav_active": "Archive"
                }),
            );
        }
    }
    
    #[cfg(feature = "console")]
    return axum::response::Html("Console not initialized".to_string()).into_response();

    #[cfg(not(feature = "console"))]
    axum::response::Html("Console feature not enabled".to_string()).into_response()
}

pub async fn list_archives(State(state): State<AppState>) -> impl IntoResponse {
    let archives = list_archive_files(&state.core.storage).await.unwrap_or_default();
    Json(archives)
}

pub async fn delete_archive(
    State(state): State<AppState>,
    Json(payload): Json<DeleteArchivePayload>,
) -> impl IntoResponse {
    let path = format!("archive/{}", payload.filename);
    // Security check: ensure no path traversal
    if payload.filename.contains("..") || payload.filename.contains("/") || payload.filename.contains("\\") {
         return Json(serde_json::json!({"success": false, "error": "Invalid filename"}));
    }

    if let Err(e) = state.core.storage.delete(&path).await {
        return Json(serde_json::json!({"success": false, "error": e.to_string()}));
    }
    Json(serde_json::json!({"success": true}))
}

pub async fn update_config(
    State(state): State<AppState>,
    Extension(archive_state): Extension<ArchiveState>,
    Json(payload): Json<UpdateConfigPayload>,
) -> impl IntoResponse {
    let config_path = state
        .config_path
        .clone()
        .unwrap_or_else(|| "config.toml".to_string());

    let res = (|| -> anyhow::Result<()> {
        let config_content = std::fs::read_to_string(&config_path)?;
        let mut doc = config_content.parse::<DocumentMut>()?;

        if !doc.contains_key("archive") {
            doc["archive"] = toml_edit::table();
        }

        let archive = &mut doc["archive"];
        archive["enabled"] = value(payload.enabled);
        archive["archive_time"] = value(&payload.archive_time);
        archive["timezone"] = value(&payload.timezone);
        archive["retention_days"] = value(payload.retention_days as i64);

        std::fs::write(&config_path, doc.to_string())?;
        info!("Updated archive config in {}", config_path);
        Ok(())
    })();

    match res {
        Ok(_) => {
            // Update in-memory config
            let mut config_guard = archive_state.config.write().unwrap();
            *config_guard = Some(crate::config::ArchiveConfig {
                enabled: payload.enabled,
                archive_time: payload.archive_time,
                timezone: Some(payload.timezone),
                retention_days: payload.retention_days,
            });
            Json(serde_json::json!({"success": true}))
        },
        Err(e) => {
            error!("Failed to update archive config: {}", e);
            Json(serde_json::json!({"success": false, "error": e.to_string()}))
        }
    }
}

async fn list_archive_files(storage: &crate::storage::Storage) -> anyhow::Result<Vec<ArchiveFile>> {
    let mut archives = Vec::new();
    let entries = storage.list(Some("archive")).await?;
    for entry in entries {
        let name = entry.location.to_string();
        if name.ends_with(".gz") {
            let filename = name.split('/').last().unwrap_or(&name).to_string();
            archives.push(ArchiveFile {
                name: filename,
                size: entry.size as u64,
                date: entry.last_modified.to_rfc3339(),
            });
        }
    }
    // Sort by date desc
    archives.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(archives)
}
