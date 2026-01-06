use crate::config::ProxyConfig;
use crate::console::handlers::{bad_request, forms, normalize_optional_string, require_field};
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::addons::queue::models::{
    ActiveModel as QueueActiveModel, Column as QueueColumn, Entity as QueueEntity,
    Model as QueueModel,
};
use crate::proxy::routing::RouteQueueConfig;
use crate::addons::queue::services::{exporter::QueueExporter, utils as queue_utils};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder,
};
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value, json};
use std::sync::Arc;
use tracing::warn;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/queues",
            get(page_queues).post(query_queues).put(create_queue),
        )
        .route("/queues/new", get(page_queue_create))
        .route("/queues/export", post(export_all_queues))
        .route(
            "/queues/{id}",
            get(page_queue_edit)
                .patch(update_queue)
                .delete(delete_queue),
        )
        .route("/queues/{id}/export", post(export_queue))
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct QueueListFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

pub async fn page_queues(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "queue.html",
        json!({
            "nav_active": "queues",
            "filters": {
                "status_options": [
                    {"value": "all", "label": "Any status"},
                    {"value": "active", "label": "Active"},
                    {"value": "inactive", "label": "Paused"},
                ],
            },
            "create_url": state.url_for("/queues/new"),
        }),
    )
}

pub async fn query_queues(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::ListQuery<QueueListFilters>>,
) -> Response {
    let db = state.db();
    let mut selector = QueueEntity::find().order_by_desc(QueueColumn::UpdatedAt);

    if let Some(filters) = &payload.filters {
        if let Some(ref raw_q) = filters.q {
            let trimmed = raw_q.trim();
            if !trimmed.is_empty() {
                let mut condition = Condition::any();
                condition = condition.add(QueueColumn::Name.contains(trimmed));
                condition = condition.add(QueueColumn::Description.contains(trimmed));
                selector = selector.filter(condition);
            }
        }
        if let Some(ref status) = filters.status {
            match status.trim().to_ascii_lowercase().as_str() {
                "active" => selector = selector.filter(QueueColumn::IsActive.eq(true)),
                "inactive" | "paused" => {
                    selector = selector.filter(QueueColumn::IsActive.eq(false))
                }
                _ => {}
            }
        }
    }

    let summary_models = match selector.clone().all(db).await {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load queue summary: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load queues"})),
            )
                .into_response();
        }
    };

    let total = summary_models.len();
    let active = summary_models
        .iter()
        .filter(|queue| queue.is_active)
        .count();
    let inactive = total.saturating_sub(active);

    let (_, per_page) = payload.normalize();
    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, &payload).await {
        Ok(result) => result,
        Err(err) => {
            warn!("failed to paginate queues: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load queues"})),
            )
                .into_response();
        }
    };

    let items: Vec<Value> = pagination
        .items
        .into_iter()
        .map(|model| queue_item_payload(state.as_ref(), &model))
        .collect();

    Json(json!({
        "page": pagination.current_page,
        "per_page": pagination.per_page,
        "total_pages": pagination.total_pages,
        "total_items": pagination.total_items,
        "items": items,
        "summary": {
            "total": total,
            "active": active,
            "inactive": inactive,
        },
        "filters": {
            "status_options": [
                {"value": "all", "label": "Any status"},
                {"value": "active", "label": "Active"},
                {"value": "inactive", "label": "Paused"},
            ],
        },
    }))
    .into_response()
}

pub async fn page_queue_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "queue_detail.html",
        json!({
            "nav_active": "queue-detail",
            "mode": "create",
            "model": {
                "spec": RouteQueueConfig::default(),
                "is_active": true,
                "tags": Vec::<String>::new(),
                "metadata_text": "",
            },
            "create_url": state.url_for("/queues"),
            "update_url": Value::Null,
            "list_url": state.url_for("/queues"),
        }),
    )
}

pub async fn page_queue_edit(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match QueueEntity::find_by_id(id).one(db).await {
        Ok(Some(queue)) => queue,
        Ok(None) => return (StatusCode::NOT_FOUND, "Queue not found").into_response(),
        Err(err) => {
            warn!("failed to load queue {} for edit: {}", id, err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load queue").into_response();
        }
    };

    let spec = queue_spec_from_model(&model);
    let metadata_text = format_metadata_text(&model.metadata);
    let tags = queue_tags(model.metadata.as_ref());

    state.render(
        "queue_detail.html",
        json!({
            "nav_active": "queue-detail",
            "mode": "edit",
            "model": {
                "id": model.id,
                "name": model.name,
                "description": model.description,
                "is_active": model.is_active,
                "spec": spec,
                "tags": tags,
                "metadata_text": metadata_text,
                "updated_at": model.updated_at.to_rfc3339(),
            },
            "create_url": state.url_for("/queues"),
            "update_url": state.url_for(&format!("/queues/{}", model.id)),
            "list_url": state.url_for("/queues"),
        }),
    )
}

pub async fn create_queue(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::QueuePayload>,
) -> Response {
    let db = state.db();
    let name = match require_field(&payload.name, "name") {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    match QueueEntity::find()
        .filter(QueueColumn::Name.eq(name.clone()))
        .one(db)
        .await
    {
        Ok(Some(_)) => return bad_request("Queue name already exists"),
        Ok(None) => {}
        Err(err) => {
            warn!("failed to enforce queue uniqueness: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create queue"})),
            )
                .into_response();
        }
    }

    let spec = payload.spec.unwrap_or_default();
    let tags = normalize_tags_list(payload.tags.clone());
    let metadata = match build_queue_metadata(payload.metadata.clone(), &tags) {
        Ok(value) => value,
        Err(resp) => return resp,
    };

    let now = Utc::now();
    let mut active: QueueActiveModel = Default::default();
    active.name = Set(name);
    active.description = Set(normalize_optional_string(&payload.description));
    active.metadata = Set(metadata);
    active.spec = Set(json!(spec));
    active.is_active = Set(payload.is_active.unwrap_or(true));
    active.created_at = Set(now);
    active.updated_at = Set(now);

    match active.insert(db).await {
        Ok(model) => {
            export_queue_async(state.as_ref(), model.id).await;
            Json(json!({"status": "ok", "id": model.id})).into_response()
        }
        Err(err) => {
            warn!("failed to insert queue: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create queue"})),
            )
                .into_response()
        }
    }
}

pub async fn update_queue(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::QueuePayload>,
) -> Response {
    let db = state.db();
    let model = match QueueEntity::find_by_id(id).one(db).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Queue not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load queue {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update queue"})),
            )
                .into_response();
        }
    };

    let requested_name = payload
        .name
        .as_ref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    if let Some(name) = &requested_name {
        if name != &model.name {
            match QueueEntity::find()
                .filter(QueueColumn::Name.eq(name.clone()))
                .one(db)
                .await
            {
                Ok(Some(other)) if other.id != id => {
                    return bad_request("Queue name already exists");
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(
                        "failed to enforce queue uniqueness on update {}: {}",
                        id, err
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"message": "Failed to update queue"})),
                    )
                        .into_response();
                }
            }
        }
    }

    let mut active: QueueActiveModel = model.into();
    if let Some(name) = requested_name {
        active.name = Set(name);
    }
    active.description = Set(normalize_optional_string(&payload.description));
    active.is_active = Set(payload.is_active.unwrap_or(true));
    active.updated_at = Set(Utc::now());

    let spec = payload.spec.unwrap_or_default();
    active.spec = Set(json!(spec));

    let tags = normalize_tags_list(payload.tags.clone());
    let metadata = match build_queue_metadata(payload.metadata.clone(), &tags) {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    active.metadata = Set(metadata);

    match active.update(db).await {
        Ok(updated) => {
            export_queue_async(state.as_ref(), updated.id).await;
            Json(json!({"status": "ok", "id": updated.id})).into_response()
        }
        Err(err) => {
            warn!("failed to update queue {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update queue"})),
            )
                .into_response()
        }
    }
}

pub async fn delete_queue(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match QueueEntity::find_by_id(id).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Queue not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load queue {} for delete: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete queue"})),
            )
                .into_response();
        }
    };

    let export_entry = queue_utils::convert_queue_model(model.clone()).ok();

    match QueueEntity::delete_by_id(id).exec(db).await {
        Ok(result) => {
            if result.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"message": "Queue not found"})),
                )
                    .into_response()
            } else {
                if let Some(entry) = export_entry {
                    remove_queue_export(state.as_ref(), entry).await;
                }
                Json(json!({"status": "ok", "rows_affected": result.rows_affected})).into_response()
            }
        }
        Err(err) => {
            warn!("failed to delete queue {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete queue"})),
            )
                .into_response()
        }
    }
}

pub async fn export_queue(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let proxy_cfg = match proxy_config_required(state.as_ref()) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };

    let exporter = QueueExporter::new(state.db().clone());
    match exporter.export_queue(id, &proxy_cfg).await {
        Ok(Some(path)) => Json(json!({"status": "ok", "path": path})).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"status": "error", "message": "Queue not found"})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to export queue {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"status": "error", "message": "Failed to export queue"})),
            )
                .into_response()
        }
    }
}

pub async fn export_all_queues(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let proxy_cfg = match proxy_config_required(state.as_ref()) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };

    let exporter = QueueExporter::new(state.db().clone());
    match exporter.export_all(&proxy_cfg).await {
        Ok(paths) => Json(json!({"status": "ok", "paths": paths})).into_response(),
        Err(err) => {
            warn!("failed to export queues: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"status": "error", "message": "Failed to export queues"})),
            )
                .into_response()
        }
    }
}

fn queue_item_payload(state: &ConsoleState, model: &QueueModel) -> Value {
    let spec = queue_spec_from_model(model);
    let tags = queue_tags(model.metadata.as_ref());
    json!({
        "id": model.id,
        "name": model.name,
        "description": model.description,
        "is_active": model.is_active,
        "spec": spec,
        "tags": tags,
        "updated_at": model.updated_at.to_rfc3339(),
        "detail_url": state.url_for(&format!("/queues/{}", model.id)),
        "delete_url": state.url_for(&format!("/queues/{}", model.id)),
        "export_url": state.url_for(&format!("/queues/{}/export", model.id)),
    })
}

fn queue_spec_from_model(model: &QueueModel) -> RouteQueueConfig {
    serde_json::from_value::<RouteQueueConfig>(model.spec.clone()).unwrap_or_default()
}

fn normalize_tags_list(tags: Option<Vec<String>>) -> Vec<String> {
    let mut results = Vec::new();
    if let Some(list) = tags {
        for tag in list {
            let cleaned = tag.trim();
            if cleaned.is_empty() {
                continue;
            }
            if results
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(cleaned))
            {
                continue;
            }
            results.push(cleaned.to_string());
        }
    }
    results
}

fn queue_tags(metadata: Option<&Value>) -> Vec<String> {
    let Some(value) = metadata else {
        return Vec::new();
    };
    value
        .get("tags")
        .and_then(|tags| serde_json::from_value::<Vec<String>>(tags.clone()).ok())
        .map(|list| normalize_tags_list(Some(list)))
        .unwrap_or_default()
}

fn build_queue_metadata(
    raw_metadata: Option<String>,
    tags: &[String],
) -> Result<Option<Value>, Response> {
    let mut map = match raw_metadata
        .as_ref()
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
    {
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(obj)) => obj,
            Ok(other) => {
                let mut wrapper = JsonMap::new();
                wrapper.insert("value".to_string(), other);
                wrapper
            }
            Err(err) => return Err(bad_request(format!("Metadata must be valid JSON: {}", err))),
        },
        None => JsonMap::new(),
    };

    if !tags.is_empty() {
        map.insert("tags".to_string(), json!(tags));
    }

    if map.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(map)))
    }
}

fn format_metadata_text(metadata: &Option<Value>) -> String {
    metadata
        .as_ref()
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
        .unwrap_or_default()
}

fn proxy_config_optional(state: &ConsoleState) -> Option<ProxyConfig> {
    state
        .app_state()
        .and_then(|app| Some(app.config().proxy.clone()))
}

fn proxy_config_required(state: &ConsoleState) -> Result<ProxyConfig, Response> {
    proxy_config_optional(state).ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            Json(json!({
                "status": "error",
                "message": "Proxy configuration is unavailable; configure proxy.generated_dir or proxy.queue_dir first."
            })),
        )
            .into_response()
    })
}

async fn export_queue_async(state: &ConsoleState, queue_id: i64) {
    let Some(proxy_cfg) = proxy_config_optional(state) else {
        warn!(
            queue_id = queue_id,
            "proxy config unavailable; skip queue export"
        );
        return;
    };
    let exporter = QueueExporter::new(state.db().clone());
    if let Err(err) = exporter.export_queue(queue_id, &proxy_cfg).await {
        warn!(queue_id = queue_id, error = %err, "failed to export queue");
    }
}

async fn remove_queue_export(state: &ConsoleState, entry: queue_utils::QueueExportEntry) {
    let Some(proxy_cfg) = proxy_config_optional(state) else {
        warn!(queue = %entry.name, "proxy config unavailable; skip queue export cleanup");
        return;
    };
    let exporter = QueueExporter::new(state.db().clone());
    if let Err(err) = exporter.remove_entry_file(&entry, &proxy_cfg).await {
        warn!(queue = %entry.name, error = %err, "failed to remove queue export file");
    }
}
