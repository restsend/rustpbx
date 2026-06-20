use crate::console::config_helpers::{
    ensure_table_mut, get_config_path, load_document, persist_document,
};
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::call_record::{Column as CallRecordColumn, Entity as CallRecordEntity};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, TimeZone};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use toml_edit::{Array, InlineTable, value};

#[derive(Debug, Deserialize)]
struct FlowQueryParams {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

#[derive(Debug, Serialize)]
struct SipFlowSettingsResponse {
    enabled: bool,
    backend_type: String,
    config: serde_json::Value,
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/sipflow/settings", get(get_settings).put(update_settings))
        .route("/sipflow/flow/{call_id}", get(query_flow))
        .route("/sipflow/media/{call_id}", get(query_media))
}

pub fn api_urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/sipflow/settings", get(get_settings).put(update_settings))
        .route("/sipflow/flow/{call_id}", get(query_flow))
        .route("/sipflow/media/{call_id}", get(query_media))
}

async fn get_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
) -> Response {
    let app_state = match state.app_state() {
        Some(app) => app,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "Application not initialized"
                })),
            )
                .into_response();
        }
    };

    let config = app_state.config();

    let (enabled, backend_type, config_json) = match &config.sipflow {
        None => (false, "none".to_string(), json!({})),
        Some(sipflow_config) => {
            use crate::config::{SipFlowClusterNode, SipFlowConfig};
            let (backend_type, config_data) = match sipflow_config {
                SipFlowConfig::Local {
                    root,
                    subdirs,
                    flush_count,
                    flush_interval_secs,
                    id_cache_size,
                    engine,
                    ttl_secs,
                    memtable_size_mb,
                    block_cache_capacity_mb,
                    ..
                } => (
                    "local",
                    json!({
                        "root": root,
                        "subdirs": subdirs,
                        "flush_count": flush_count,
                        "flush_interval_secs": flush_interval_secs,
                        "id_cache_size": id_cache_size,
                        "engine": engine,
                        "ttl_secs": ttl_secs,
                        "memtable_size_mb": memtable_size_mb,
                        "block_cache_capacity_mb": block_cache_capacity_mb
                    }),
                ),
                SipFlowConfig::Remote {
                    nodes,
                    udp_addr,
                    http_addr,
                    timeout_secs,
                    ..
                } => {
                    let mut resolved = nodes.clone();
                    if resolved.is_empty() {
                        if let (Some(udp), Some(http)) = (udp_addr, http_addr) {
                            resolved.push(SipFlowClusterNode {
                                udp: udp.clone(),
                                http: http.clone(),
                            });
                        }
                    }
                    (
                        "remote",
                        json!({
                            "nodes": resolved,
                            "timeout_secs": timeout_secs
                        }),
                    )
                }
            };
            (true, backend_type.to_string(), config_data)
        }
    };

    Json(SipFlowSettingsResponse {
        enabled,
        backend_type,
        config: config_json,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct UpdateSettingsRequest {
    backend_type: String,
    config: serde_json::Value,
}

async fn update_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    Json(payload): Json<UpdateSettingsRequest>,
) -> Response {
    // Get config file path
    let config_path = match get_config_path(&state) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    // Load TOML document
    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    // Ensure [sipflow] section exists
    let table = ensure_table_mut(&mut doc, "sipflow");

    // Update backend type
    let backend_type = payload.backend_type.as_str();
    match backend_type {
        "none" => {
            doc.remove("sipflow");
        }
        "local" => {
            table["type"] = value("local");
            if let Some(root) = payload.config.get("root").and_then(|v| v.as_str()) {
                table["root"] = value(root);
            }
            if let Some(subdirs) = payload.config.get("subdirs").and_then(|v| v.as_str()) {
                table["subdirs"] = value(subdirs);
            }
            if let Some(count) = payload.config.get("flush_count").and_then(|v| v.as_i64()) {
                table["flush_count"] = value(count);
            }
            if let Some(secs) = payload
                .config
                .get("flush_interval_secs")
                .and_then(|v| v.as_i64())
            {
                table["flush_interval_secs"] = value(secs);
            }
            // Remove other backend fields
            table.remove("udp_addr");
            table.remove("http_addr");
            table.remove("timeout_secs");
        }
        "remote" => {
            table["type"] = value("remote");
            let has_nodes = payload
                .config
                .get("nodes")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if has_nodes {
                if let Some(nodes) = payload.config.get("nodes").and_then(|v| v.as_array()) {
                    let arr: Array = nodes
                        .iter()
                        .filter_map(|n| {
                            let udp = n.get("udp")?.as_str()?;
                            let http = n.get("http")?.as_str()?;
                            let mut t = InlineTable::new();
                            t.insert("udp", udp.into());
                            t.insert("http", http.into());
                            Some(toml_edit::Value::from(t))
                        })
                        .collect();
                    table["nodes"] = value(arr);
                }
                table.remove("udp_addr");
                table.remove("http_addr");
            } else {
                if let Some(addr) = payload.config.get("udp_addr").and_then(|v| v.as_str()) {
                    table["udp_addr"] = value(addr);
                }
                if let Some(addr) = payload.config.get("http_addr").and_then(|v| v.as_str()) {
                    table["http_addr"] = value(addr);
                }
                table.remove("nodes");
            }
            if let Some(secs) = payload.config.get("timeout_secs").and_then(|v| v.as_i64()) {
                table["timeout_secs"] = value(secs);
            }
            // Remove other backend fields
            table.remove("root");
            table.remove("subdirs");
            table.remove("flush_count");
            table.remove("flush_interval_secs");
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("Invalid backend type: {}", backend_type)
                })),
            )
                .into_response();
        }
    }

    // Write back to file
    let doc_text = doc.to_string();
    if let Err(resp) = persist_document(&config_path, doc_text) {
        return resp;
    }

    (
        StatusCode::OK,
        Json(json!({
            "message": "SipFlow settings updated. Please restart the server for changes to take effect.",
            "restart_required": true
        })),
    )
        .into_response()
}

async fn query_flow(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    AxumPath(call_id): AxumPath<String>,
    Query(params): Query<FlowQueryParams>,
) -> Response {
    let sip_server = match state.sip_server() {
        Some(server) => server,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "SIP server not available"
                })),
            )
                .into_response();
        }
    };

    let sipflow = match &sip_server.sip_flow {
        Some(flow) => flow,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "SipFlow not enabled"
                })),
            )
                .into_response();
        }
    };

    let backend = match sipflow.backend() {
        Some(backend) => backend,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "SipFlow backend not configured"
                })),
            )
                .into_response();
        }
    };

    // Parse time range
    let now = chrono::Local::now();
    let mut start_time = params.start.and_then(|s| parse_datetime(&s));
    let mut end_time = params.end.and_then(|s| parse_datetime(&s));

    if (start_time.is_none() || end_time.is_none())
        && let Ok(Some(record)) = CallRecordEntity::find()
            .filter(CallRecordColumn::CallId.eq(&call_id))
            .one(state.db())
            .await
    {
        if start_time.is_none() {
            start_time = Some(
                record.started_at.with_timezone(&chrono::Local) - chrono::Duration::minutes(10),
            );
        }
        if end_time.is_none() {
            end_time = Some(
                record
                    .ended_at
                    .unwrap_or(record.started_at)
                    .with_timezone(&chrono::Local)
                    + chrono::Duration::hours(1),
            );
        }
    }

    let start_time = start_time.unwrap_or_else(|| now - chrono::Duration::hours(1));
    let end_time = end_time.unwrap_or(now);

    match backend.query_flow(&call_id, start_time, end_time).await {
        Ok(items) => {
            if items.is_empty() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": "Call flow not found"
                    })),
                )
                    .into_response();
            }

            let json_items: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    json!({
                        "seq": item.seq,
                        "timestamp": item.timestamp,
                        "msg_type": format!("{:?}", item.msg_type),
                        "src_addr": item.src_addr,
                        "dst_addr": item.dst_addr,
                        "raw_message": String::from_utf8_lossy(&item.payload),
                    })
                })
                .collect();

            Json(json!({
                "status": "success",
                "call_id": call_id,
                "start_time": start_time.to_rfc3339(),
                "end_time": end_time.to_rfc3339(),
                "flow": json_items
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!("Failed to query flow: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn query_media(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    AxumPath(call_id): AxumPath<String>,
    Query(params): Query<FlowQueryParams>,
) -> Response {
    let sip_server = match state.sip_server() {
        Some(server) => server,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "SIP server not available"
                })),
            )
                .into_response();
        }
    };

    let sipflow = match &sip_server.sip_flow {
        Some(flow) => flow,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "SipFlow not enabled"
                })),
            )
                .into_response();
        }
    };

    let backend = match sipflow.backend() {
        Some(backend) => backend,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "SipFlow backend not configured"
                })),
            )
                .into_response();
        }
    };

    let now = chrono::Local::now();
    let mut start_time = params.start.and_then(|s| parse_datetime(&s));
    let mut end_time = params.end.and_then(|s| parse_datetime(&s));

    if (start_time.is_none() || end_time.is_none())
        && let Ok(Some(record)) = CallRecordEntity::find()
            .filter(CallRecordColumn::CallId.eq(&call_id))
            .one(state.db())
            .await
    {
        if start_time.is_none() {
            start_time = Some(
                record.started_at.with_timezone(&chrono::Local) - chrono::Duration::minutes(10),
            );
        }
        if end_time.is_none() {
            end_time = Some(
                record
                    .ended_at
                    .unwrap_or(record.started_at)
                    .with_timezone(&chrono::Local)
                    + chrono::Duration::hours(1),
            );
        }
    }

    let start_time = start_time.unwrap_or_else(|| now - chrono::Duration::hours(1));
    let end_time = end_time.unwrap_or(now);

    match backend
        .generate_wav_file(&call_id, start_time, end_time, None)
        .await
    {
        Ok(temp_file) => {
            let temp_path = temp_file.path().to_owned();
            let file_len = match std::fs::metadata(&temp_path) {
                Ok(m) => m.len(),
                Err(_) => 0,
            };

            if file_len <= 44 {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": "Call media not found"
                    })),
                )
                    .into_response();
            }

            use axum::http::header;
            use tokio_util::io::ReaderStream;

            let file = match tokio::fs::File::open(&temp_path).await {
                Ok(f) => f,
                Err(_) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": "Failed to open temp file" })),
                    )
                        .into_response();
                }
            };
            let path_str = temp_path.to_string_lossy().to_string();
            let _tmp_path = temp_file.into_temp_path();

            let stream = ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);

            let response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "audio/wav")
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}.wav\"", call_id),
                )
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

            let _ = std::fs::remove_file(&path_str);
            response
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": format!("Failed to query media: {}", e)
            })),
        )
            .into_response(),
    }
}

fn parse_datetime(s: &str) -> Option<DateTime<chrono::Local>> {
    // Try ISO 8601 format
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Local));
    }

    // Try Unix timestamp
    if let Ok(ts) = s.parse::<i64>()
        && let Some(dt) = chrono::Local.timestamp_opt(ts, 0).single()
    {
        return Some(dt);
    }

    None
}
