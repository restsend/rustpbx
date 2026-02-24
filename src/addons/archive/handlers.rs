use super::{ArchiveAddon, ArchiveState, ManualTaskStatus};
use crate::app::AppState;
use axum::{
    Extension,
    extract::{Json, Path, Query, State},
    response::IntoResponse,
};
use chrono::NaiveDate;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use toml_edit::{DocumentMut, value};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Serialize)]
pub struct ArchiveFile {
    name: String,
    size: u64,
    date: String,
    count: Option<u64>,
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
    /// Empty string means "use default"; omitted/None also means default.
    #[serde(default)]
    archive_dir: Option<String>,
}

pub async fn ui_index(
    State(state): State<AppState>,
    Extension(archive_state): Extension<ArchiveState>,
) -> impl IntoResponse {
    #[cfg(feature = "console")]
    {
        if let Some(console) = &state.console {
            let archive_dir = state.config().archive_dir();
            let archives = list_archive_files(&archive_dir).await.unwrap_or_default();
            let config = archive_state.config.read().unwrap().clone();
            let effective_archive_dir = state.config().archive_dir();
            return console.render(
                "archive/archive_index.html",
                serde_json::json!({
                    "archives": archives,
                    "config": config,
                    "effective_archive_dir": effective_archive_dir,
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
    let archive_dir = state.config().archive_dir();
    let archives = list_archive_files(&archive_dir).await.unwrap_or_default();
    Json(archives)
}

pub async fn delete_archive(
    State(state): State<AppState>,
    Json(payload): Json<DeleteArchivePayload>,
) -> impl IntoResponse {
    // Security check: ensure no path traversal
    if payload.filename.contains("..")
        || payload.filename.contains("/")
        || payload.filename.contains("\\")
    {
        return Json(serde_json::json!({"success": false, "error": "Invalid filename"}));
    }

    let archive_dir = state.config().archive_dir();
    let full_path = format!("{}/{}", archive_dir, payload.filename);
    if let Err(e) = tokio::fs::remove_file(&full_path).await {
        return Json(serde_json::json!({"success": false, "error": e.to_string()}));
    }
    // Best-effort delete the count sidecar
    let _ = tokio::fs::remove_file(format!("{}.count", full_path)).await;
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

    // Validate timezone before touching the config file
    let tz_str = payload.timezone.trim();
    if tz_str.parse::<chrono_tz::Tz>().is_err() {
        return Json(
            serde_json::json!({"success": false, "error": format!("Invalid timezone '{}'. Use IANA format, e.g. Asia/Shanghai, America/New_York, UTC.", tz_str)})
        ).into_response();
    }

    // Validate archive_time format HH:MM
    if chrono::NaiveTime::parse_from_str(payload.archive_time.trim(), "%H:%M").is_err() {
        return Json(
            serde_json::json!({"success": false, "error": format!("Invalid archive_time '{}'. Expected HH:MM format, e.g. 03:00.", payload.archive_time.trim())})
        ).into_response();
    }

    let res = (|| -> anyhow::Result<()> {
        let config_content = std::fs::read_to_string(&config_path)?;
        let mut doc = config_content.parse::<DocumentMut>()?;

        if !doc.contains_key("archive") {
            doc["archive"] = toml_edit::table();
        }

        let archive = &mut doc["archive"];
        archive["enabled"] = value(payload.enabled);
        archive["archive_time"] = value(payload.archive_time.trim());
        archive["timezone"] = value(tz_str);
        archive["retention_days"] = value(payload.retention_days as i64);
        match payload.archive_dir.as_deref() {
            Some(d) if !d.trim().is_empty() => {
                archive["archive_dir"] = value(d.trim());
            }
            _ => {
                // Remove override → fall back to derived default
                if let Some(t) = doc["archive"].as_table_mut() {
                    t.remove("archive_dir");
                }
            }
        }
        std::fs::write(&config_path, doc.to_string())?;
        info!("Updated archive config in {}", config_path);
        Ok(())
    })();

    match res {
        Ok(_) => {
            // Update in-memory config
            let tz_str = tz_str.to_string();
            let mut config_guard = archive_state.config.write().unwrap();
            let new_archive_dir = payload
                .archive_dir
                .as_deref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            *config_guard = Some(crate::config::ArchiveConfig {
                enabled: payload.enabled,
                archive_time: payload.archive_time.trim().to_string(),
                timezone: Some(tz_str.to_string()),
                retention_days: payload.retention_days,
                archive_dir: new_archive_dir,
            });
            Json(serde_json::json!({"success": true})).into_response()
        }
        Err(e) => {
            error!("Failed to update archive config: {}", e);
            Json(serde_json::json!({"success": false, "error": e.to_string()})).into_response()
        }
    }
}

async fn list_archive_files(archive_dir: &str) -> anyhow::Result<Vec<ArchiveFile>> {
    use chrono::{DateTime, Utc};
    use std::time::SystemTime;

    let mut archives = Vec::new();
    let mut read_dir = match tokio::fs::read_dir(archive_dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(archives),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = read_dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".gz") {
            continue;
        }
        let meta = entry.metadata().await?;
        let modified: DateTime<Utc> = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH).into();
        // Read sidecar count file
        let count_path = format!("{}/{}.count", archive_dir, name);
        let count = tokio::fs::read_to_string(&count_path)
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        archives.push(ArchiveFile {
            name,
            size: meta.len(),
            date: modified.to_rfc3339(),
            count,
        });
    }
    // Sort by date desc
    archives.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(archives)
}

// ─── Manual archive endpoints ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DateRangeQuery {
    pub start_date: String,
    pub end_date: String,
}

#[derive(Deserialize)]
pub struct ManualArchivePayload {
    pub start_date: String,
    pub end_date: String,
}

pub async fn count_records(
    State(state): State<AppState>,
    Query(params): Query<DateRangeQuery>,
) -> impl IntoResponse {
    let start = match params.start_date.parse::<NaiveDate>() {
        Ok(d) => d,
        Err(_) => {
            return Json(serde_json::json!({"success": false, "error": "Invalid start_date"}));
        }
    };
    let end = match params.end_date.parse::<NaiveDate>() {
        Ok(d) => d,
        Err(_) => return Json(serde_json::json!({"success": false, "error": "Invalid end_date"})),
    };
    if end <= start {
        return Json(
            serde_json::json!({"success": false, "error": "end_date must be after start_date"}),
        );
    }

    use crate::models::call_record;
    use chrono::{DateTime, Utc};

    let start_dt: DateTime<Utc> =
        DateTime::from_naive_utc_and_offset(start.and_hms_opt(0, 0, 0).unwrap(), Utc);
    let end_dt: DateTime<Utc> =
        DateTime::from_naive_utc_and_offset(end.and_hms_opt(0, 0, 0).unwrap(), Utc);

    match call_record::Entity::find()
        .filter(call_record::Column::StartedAt.gte(start_dt))
        .filter(call_record::Column::StartedAt.lt(end_dt))
        .count(state.db())
        .await
    {
        Ok(count) => Json(serde_json::json!({"success": true, "count": count})),
        Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
    }
}

pub async fn manual_archive(
    State(state): State<AppState>,
    Extension(archive_state): Extension<ArchiveState>,
    Json(payload): Json<ManualArchivePayload>,
) -> impl IntoResponse {
    let start = match payload.start_date.parse::<NaiveDate>() {
        Ok(d) => d,
        Err(_) => {
            return Json(serde_json::json!({"success": false, "error": "Invalid start_date"}));
        }
    };
    let end = match payload.end_date.parse::<NaiveDate>() {
        Ok(d) => d,
        Err(_) => return Json(serde_json::json!({"success": false, "error": "Invalid end_date"})),
    };
    if end <= start {
        return Json(
            serde_json::json!({"success": false, "error": "end_date must be after start_date"}),
        );
    }

    let task_id = Uuid::new_v4().to_string();
    let task_status = Arc::new(RwLock::new(ManualTaskStatus {
        status: "running".to_string(),
        archived: 0,
        total: 0,
        message: "Starting...".to_string(),
        completed_at: None,
    }));

    // Prune tasks that finished more than 1 hour ago to prevent unbounded growth
    {
        let ttl = std::time::Duration::from_secs(3600);
        let mut tasks = archive_state.manual_tasks.write().unwrap();
        tasks.retain(|_, v| {
            let s = v.read().unwrap();
            s.completed_at.map_or(true, |t| t.elapsed() < ttl)
        });
    }

    archive_state
        .manual_tasks
        .write()
        .unwrap()
        .insert(task_id.clone(), task_status.clone());

    let archive_dir = state.config().archive_dir();
    let db = state.db().clone();

    crate::utils::spawn(async move {
        if let Err(e) =
            ArchiveAddon::perform_archive_range(&db, start, end, &archive_dir, task_status.clone())
                .await
        {
            error!("Manual archive failed: {}", e);
            let mut s = task_status.write().unwrap();
            s.status = "error".to_string();
            s.message = e.to_string();
            s.completed_at = Some(std::time::Instant::now());
        }
    });

    Json(serde_json::json!({"success": true, "task_id": task_id}))
}

pub async fn task_status(
    Extension(archive_state): Extension<ArchiveState>,
    Path(task_id): Path<String>,
) -> impl IntoResponse {
    let tasks = archive_state.manual_tasks.read().unwrap();
    match tasks.get(&task_id) {
        Some(task) => {
            let s = task.read().unwrap();
            Json(serde_json::json!({
                "success": true,
                "status": s.status,
                "archived": s.archived,
                "total": s.total,
                "message": s.message,
            }))
        }
        None => Json(serde_json::json!({"success": false, "error": "Task not found"})),
    }
}
