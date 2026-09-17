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
    /// Set to `0`/`false` to skip the pre-query flush (stale-but-fast reads).
    /// By default the pipeline is flushed first so a query issued right after
    /// a call ends sees every message.
    #[serde(default)]
    flush: Option<String>,
}

impl FlowQueryParams {
    fn flush_enabled(&self) -> bool {
        !matches!(self.flush.as_deref(), Some("0") | Some("false"))
    }
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
                    compress,
                    compress_level,
                    shards,
                    upload,
                    ..
                } => (
                    "local",
                    json!({
                        "root": root,
                        "subdirs": subdirs,
                        "flush_count": flush_count,
                        "flush_interval_secs": flush_interval_secs,
                        "id_cache_size": id_cache_size,
                        "compress": compress,
                        "compress_level": compress_level,
                        "shards": shards,
                        "upload": sipflow_upload_json(upload.as_ref()),
                    }),
                ),
                SipFlowConfig::Remote {
                    nodes,
                    udp_addr,
                    http_addr,
                    timeout_secs,
                    upload,
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
                            "timeout_secs": timeout_secs,
                            "upload": sipflow_upload_json(upload.as_ref()),
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

/// Serialize the optional `[sipflow.upload]` section for the console UI.
/// Secrets are never exposed; only whether credentials are configured is sent.
fn sipflow_upload_json(upload: Option<&crate::config::SipFlowUploadConfig>) -> serde_json::Value {
    use crate::config::SipFlowUploadConfig;
    match upload {
        None => json!(null),
        Some(SipFlowUploadConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            root,
            signaling,
            media,
            ..
        }) => json!({
            "type": "s3",
            "vendor": vendor,
            "bucket": bucket,
            "region": region,
            "endpoint": endpoint,
            "root": root,
            "media": media,
            "signaling": signaling,
            "has_credentials": access_key.is_some() || secret_key.is_some(),
        }),
        Some(SipFlowUploadConfig::Http {
            url,
            signaling,
            media,
            ..
        }) => json!({
            "type": "http",
            "url": url,
            "media": media,
            "signaling": signaling,
        }),
    }
}

/// Apply the optional `upload` object from an [`UpdateSettingsRequest`] to the
/// `[sipflow]` table. A missing/null upload removes the section. Credentials are
/// optional: omitting both access and secret key selects anonymous/public
/// access to the bucket.
fn apply_sipflow_upload(
    table: &mut toml_edit::Table,
    config: &serde_json::Value,
) -> Result<(), String> {
    let Some(upload) = config.get("upload").filter(|value| !value.is_null()) else {
        table.remove("upload");
        return Ok(());
    };

    let mut upload_table = InlineTable::new();
    match upload.get("type").and_then(|value| value.as_str()) {
        Some("s3") => {
            let vendor = upload
                .get("vendor")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim();
            let bucket = upload
                .get("bucket")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim();
            let region = upload
                .get("region")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim();
            let endpoint = upload
                .get("endpoint")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim();
            if vendor.is_empty() || bucket.is_empty() || region.is_empty() || endpoint.is_empty() {
                return Err(
                    "S3 upload requires vendor, bucket, region, and endpoint".to_string(),
                );
            }
            upload_table.insert("type", "s3".into());
            upload_table.insert("vendor", vendor.into());
            upload_table.insert("bucket", bucket.into());
            upload_table.insert("region", region.into());
            upload_table.insert("endpoint", endpoint.into());
            let root = upload
                .get("root")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("sipflow");
            upload_table.insert("root", root.into());
            if let Some(access_key) = upload
                .get("access_key")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                upload_table.insert("access_key", access_key.into());
            }
            if let Some(secret_key) = upload
                .get("secret_key")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                upload_table.insert("secret_key", secret_key.into());
            }
            if let Some(media) = upload.get("media").and_then(|value| value.as_bool()) {
                upload_table.insert("media", media.into());
            }
            if let Some(signaling) = upload.get("signaling").and_then(|value| value.as_bool()) {
                upload_table.insert("signaling", signaling.into());
            }
        }
        Some("http") => {
            let url = upload
                .get("url")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim();
            if url.is_empty() {
                return Err("HTTP upload requires a url".to_string());
            }
            upload_table.insert("type", "http".into());
            upload_table.insert("url", url.into());
            if let Some(media) = upload.get("media").and_then(|value| value.as_bool()) {
                upload_table.insert("media", media.into());
            }
            if let Some(signaling) = upload.get("signaling").and_then(|value| value.as_bool()) {
                upload_table.insert("signaling", signaling.into());
            }
        }
        other => {
            let label = other.unwrap_or("<missing>");
            return Err(format!("Invalid upload type: {label}"));
        }
    }

    table["upload"] = toml_edit::value(upload_table);
    Ok(())
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
            table.remove("engine");
            table.remove("ttl_secs");
            table.remove("memtable_size_mb");
            table.remove("block_cache_capacity_mb");
            table.remove("flowdb_sync_mode");
            if let Err(err) = apply_sipflow_upload(table, &payload.config) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": err })),
                )
                    .into_response();
            }
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
            table.remove("engine");
            table.remove("ttl_secs");
            table.remove("memtable_size_mb");
            table.remove("block_cache_capacity_mb");
            table.remove("flowdb_sync_mode");
            if let Err(err) = apply_sipflow_upload(table, &payload.config) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": err })),
                )
                    .into_response();
            }
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

    // Hot-reload the SipFlow backend without restarting the service.
    let mut reload_message = String::new();
    let mut reload_error = Option::<String>::None;
    if let Some(app_state) = state.app_state() {
        let inner = &app_state.sip_server().inner;
        match inner.reload_sipflow(&config_path).await {
            Ok(msg) => reload_message = msg,
            Err(e) => reload_error = Some(e.to_string()),
        }
    }

    if let Some(err) = reload_error {
        Json(json!({
            "status": "ok",
            "message": format!("SipFlow settings saved but apply failed: {err}"),
            "restart_required": false
        }))
        .into_response()
    } else {
        Json(json!({
            "status": "ok",
            "message": format!("SipFlow settings applied. {reload_message}"),
            "restart_required": false
        }))
        .into_response()
    }
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
    let flush_enabled = params.flush_enabled();
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

    // A query issued right after a call ends must see the tail messages
    // still in the write pipeline: flush first (bounded), then query.
    if flush_enabled {
        crate::callrecord::sipflow::flush_with_deadline(sipflow).await;
    }

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
            let file_len = match tokio::fs::metadata(&temp_path).await {
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
                    format!(
                        "attachment; filename=\"{}.wav\"",
                        crate::utils::sanitize_header_value(&call_id)
                    ),
                )
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

            let _ = tokio::fs::remove_file(&path_str).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(config: serde_json::Value) -> Result<toml_edit::Table, String> {
        let mut table = toml_edit::Table::new();
        apply_sipflow_upload(&mut table, &config)?;
        Ok(table)
    }

    #[test]
    fn s3_upload_without_credentials_omits_keys_and_deserializes() {
        // Mirror the handler's write order: backend fields first, upload last.
        let mut table = toml_edit::Table::new();
        table["type"] = value("local");
        table["root"] = value("./sipflow");
        apply_sipflow_upload(
            &mut table,
            &json!({
                "upload": {
                    "type": "s3",
                    "vendor": "minio",
                    "bucket": "public",
                    "region": "us-east-1",
                    "endpoint": "http://127.0.0.1:9000",
                    "root": "sipflow",
                    "media": true,
                    "signaling": true,
                }
            }),
        )
        .expect("apply");

        let text = table.to_string();
        assert!(!text.contains("access_key"), "unexpected credential: {text}");
        assert!(!text.contains("secret_key"), "unexpected credential: {text}");

        #[derive(serde::Deserialize)]
        struct Wrapper {
            sipflow: crate::config::SipFlowConfig,
        }
        let mut doc = toml_edit::DocumentMut::new();
        doc["sipflow"] = toml_edit::Item::Table(table);
        let parsed: crate::config::SipFlowConfig =
            toml::from_str::<Wrapper>(&doc.to_string())
                .expect("roundtrip")
                .sipflow;
        let crate::config::SipFlowConfig::Local {
            upload: Some(upload),
            ..
        } = parsed
        else {
            panic!("expected local config with upload");
        };
        let crate::config::SipFlowUploadConfig::S3 {
            access_key,
            secret_key,
            ..
        } = upload
        else {
            panic!("expected s3 upload");
        };
        assert_eq!(access_key, None);
        assert_eq!(secret_key, None);
    }

    #[test]
    fn s3_upload_with_credentials_persists_keys() {
        let table = apply(json!({
            "upload": {
                "type": "s3",
                "vendor": "minio",
                "bucket": "private",
                "region": "us-east-1",
                "endpoint": "http://127.0.0.1:9000",
                "root": "sipflow",
                "access_key": "ak",
                "secret_key": "sk",
            }
        }))
        .expect("apply");

        let text = table.to_string();
        assert!(text.contains("access_key = \"ak\""), "{text}");
        assert!(text.contains("secret_key = \"sk\""), "{text}");
    }

    #[test]
    fn missing_upload_removes_section() {
        let mut table = toml_edit::Table::new();
        table["upload"] = toml_edit::value(toml_edit::InlineTable::new());

        apply_sipflow_upload(&mut table, &json!({})).expect("apply");

        assert!(!table.contains_key("upload"));
    }

    #[test]
    fn invalid_s3_upload_rejected() {
        assert!(apply(json!({"upload": {"type": "s3", "vendor": "minio"}})).is_err());
    }
}
