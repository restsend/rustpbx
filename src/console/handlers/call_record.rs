use crate::callrecord::CallRecord;
use crate::callrecord::storage;
use crate::callrecord::storage::CdrStorage;
use crate::console::config_helpers::{bad_request, find_or_404, internal_error};
use crate::console::{ConsoleState, handlers::forms, middleware::AuthRequired};
use crate::storage::Storage;
use crate::models::{
    call_record::{
        ActiveModel as CallRecordActiveModel, Column as CallRecordColumn,
        Entity as CallRecordEntity, Model as CallRecordModel,
    },
    department::{
        Column as DepartmentColumn, Entity as DepartmentEntity, Model as DepartmentModel,
    },
    extension::{Entity as ExtensionEntity, Model as ExtensionModel},
    sip_trunk::{Column as SipTrunkColumn, Entity as SipTrunkEntity, Model as SipTrunkModel},
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{self, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sea_orm::sea_query::Order;
use sea_orm::QuerySelect;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection, DbErr,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tracing::{debug, warn};

use crate::media::wav_reader::{WavReader, WavSpec, WavWriter};

const OUTBOUND_TRUNK_NAME_KEY: &str = "outbound_trunk_name";
const OUTBOUND_TRUNK_DEST_KEY: &str = "outbound_trunk_dest";

/// Local recording artifacts are archived into `{root}/{YYYYMMDD}[/{HH}]`
/// subdirectories after the call completes. Rows persisted before that rename
/// (e.g. written by older builds) store the pre-archive path, so try the
/// recorded path first and fall back to the daily/hourly layouts derived
/// from the call's start time.
fn resolve_archived_artifact_path(path: &str, at: DateTime<Utc>) -> String {
    if path.is_empty() || Path::new(path).exists() {
        return path.to_string();
    }
    let Some(root) = Path::new(path).parent() else {
        return path.to_string();
    };
    let Some(name) = Path::new(path).file_name() else {
        return path.to_string();
    };
    let candidates = [
        crate::callrecord::RecordingSubdir::Daily.relative_dir(at),
        crate::callrecord::RecordingSubdir::Hourly.relative_dir(at),
    ];
    for dir in candidates {
        let candidate = root.join(dir).join(name);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueryCallRecordFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    date_from: Option<String>,
    #[serde(default)]
    date_to: Option<String>,
    #[serde(default)]
    only_transcribed: Option<bool>,
    #[serde(default)]
    department_ids: Option<Vec<i64>>,
    #[serde(default)]
    sip_trunk_ids: Option<Vec<i64>>,
    #[serde(default)]
    outbound_sip_trunk_ids: Option<Vec<i64>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    caller: Option<String>,
    #[serde(default)]
    callee: Option<String>,
    /// `false` (default): only primary CDRs — one row per logical call.
    /// `true`: include every child leg (queue dispatch, REFER transfer).
    #[serde(default)]
    all_legs: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RecordingPlaybackQuery {
    #[serde(default)]
    stream: Option<String>,
    /// Select one recording segment of a segmented call: the media entry's
    /// `unique_id` (preferred, same id as `recording_metadata_available`)
    /// or its `track_id` as fallback. Absent → the first existing file.
    #[serde(default)]
    segment: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SipFlowRequestQuery {
    #[serde(default)]
    detail: bool,
    /// Set to `0`/`false` to skip the pre-query flush (stale-but-fast reads).
    #[serde(default)]
    flush: Option<bool>,
}

impl SipFlowRequestQuery {
    fn flush_enabled(&self) -> bool {
        self.flush.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCallRecordPayload {
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    note: Option<NotePayload>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotePayload {
    text: Option<String>,
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/call-records",
            get(page_call_records).post(query_call_records),
        )
        .route(
            "/call-records/{id}",
            get(page_call_record_detail)
                .patch(update_call_record)
                .delete(delete_call_record),
        )
        .route(
            "/call-records/{id}/sip-flow",
            get(download_call_record_sip_flow),
        )
        .route(
            "/call-records/{id}/cdr-json",
            get(download_call_record_cdr_json),
        )
        .route("/call-records/{id}/recording", get(stream_call_recording))
        .route(
            "/call-records/by-session/{session_id}/artifacts",
            get(list_session_artifacts),
        )
}

pub fn api_urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/call-records",
            get(query_call_records).post(query_call_records),
        )
        .route("/call-records/export", get(export_call_records_csv))
        .route(
            "/call-records/{id}",
            patch(update_call_record).delete(delete_call_record),
        )
        .route(
            "/call-records/{id}/sip-flow",
            get(download_call_record_sip_flow),
        )
        .route(
            "/call-records/{id}/cdr-json",
            get(download_call_record_cdr_json),
        )
        .route("/call-records/{id}/recording", get(stream_call_recording))
        .route(
            "/call-records/by-session/{session_id}/artifacts",
            get(list_session_artifacts),
        )
}

/// Hard cap for a single CSV export — prevents one request from pulling the
/// whole table through memory.
const EXPORT_MAX_ROWS: u64 = 50_000;

/// CSV export of the CDR list with the same filters as the console list
/// view. Batched fetch (cursor by id) so a large result does not need to be
/// materialized as sea-orm models all at once.
pub(crate) async fn export_call_records_csv(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Query(filters): Query<QueryCallRecordFilters>,
) -> Response {
    let cdr_date = filters.date_from.as_deref();
    let cdb = state.cdr_db(cdr_date).await;
    let condition = build_condition(&Some(filters));

    let mut csv = String::from(
        "call_id,session_id,direction,status,started_at,ended_at,duration_secs,from_number,to_number,agent_name,queue,department_id,sip_trunk_id,hangup_reason,sip_status_code,recording_url\n",
    );

    let mut last_id: i64 = 0;
    let mut exported: u64 = 0;
    loop {
        let mut selector = CallRecordEntity::find()
            .filter(condition.clone())
            .order_by_asc(CallRecordColumn::Id)
            .limit(1000);
        if last_id > 0 {
            selector = selector.filter(CallRecordColumn::Id.gt(last_id));
        }
        let batch = match selector.all(&cdb).await {
            Ok(b) => b,
            Err(err) => {
                warn!("failed to export call records: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "message": err.to_string() })),
                )
                    .into_response();
            }
        };
        if batch.is_empty() || exported >= EXPORT_MAX_ROWS {
            break;
        }
        for r in &batch {
            last_id = r.id;
            exported += 1;
            if exported > EXPORT_MAX_ROWS {
                break;
            }
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},\n",
                csv_escape(&r.call_id),
                csv_escape(r.session_id.as_deref().unwrap_or("")),
                csv_escape(&r.direction),
                csv_escape(&r.status),
                csv_escape(&r.started_at.to_rfc3339()),
                csv_escape(
                    &r.ended_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default()
                ),
                r.duration_secs,
                csv_escape(r.from_number.as_deref().unwrap_or("")),
                csv_escape(r.to_number.as_deref().unwrap_or("")),
                csv_escape(r.agent_name.as_deref().unwrap_or("")),
                csv_escape(r.queue.as_deref().unwrap_or("")),
                r.department_id.map(|v| v.to_string()).unwrap_or_default(),
                r.sip_trunk_id.map(|v| v.to_string()).unwrap_or_default(),
                csv_escape(r.hangup_reason.as_deref().unwrap_or("")),
                r.sip_status_code
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                csv_escape(r.recording_url.as_deref().unwrap_or("")),
            ));
        }
        if batch.len() < 1000 {
            break;
        }
    }

    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"call_records.csv\"",
            ),
        ],
        csv,
    )
        .into_response()
}

/// RFC4180: quote fields containing comma, quote, or newline; double any
/// embedded quotes.
fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

async fn resolve_call_record_by_id_or_call_id(
    db: &sea_orm::DatabaseConnection,
    identifier: &str,
) -> Result<crate::models::call_record::Model, Response> {
    // Try to lookup by integer ID first
    if let Ok(id) = identifier.parse::<i64>() {
        match CallRecordEntity::find_by_id(id).one(db).await {
            Ok(Some(model)) => return Ok(model),
            Ok(None) => {}
            Err(err) => {
                warn!(id = id, "failed to load call record: {}", err);
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "message": format!("Failed to load call record: {}", err) })),
                )
                    .into_response());
            }
        }
    }

    // Fallback to lookup by call_id
    match CallRecordEntity::find()
        .filter(crate::models::call_record::Column::CallId.eq(identifier))
        .one(db)
        .await
    {
        Ok(Some(model)) => Ok(model),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({
                "message": format!("Call record not found for identifier: {}", identifier),
                "hint": "Provide either the numeric call record ID or the SIP Call-ID."
            })),
        )
            .into_response()),
        Err(err) => {
            warn!(call_id = %identifier, "failed to load call record by call_id: {}", err);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": format!("Failed to load call record: {}", err) })),
            )
                .into_response())
        }
    }
}

/// Download the entire CDR JSON of a call record as an attachment.
/// The record may be addressed by numeric id or SIP Call-ID.
///
/// Serves the archived CDR JSON file when available; when the file is
/// missing (e.g. `[callrecord]` storage disabled, or the artifact was
/// pruned), falls back to synthesizing the CDR JSON from the database row.
async fn download_call_record_cdr_json(
    AxumPath(identifier): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let record = match resolve_call_record_by_id_or_call_id(state.db(), &identifier).await {
        Ok(record) => record,
        Err(response) => return response,
    };

    let (raw, filename) = if let Some((raw, path, _storage)) = read_cdr_raw(&state, &record).await {
        let filename = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("cdr.json")
            .to_string();
        (raw, filename)
    } else {
        let callrecord: CallRecord = record.clone().into();
        let raw = match serde_json::to_string_pretty(&callrecord) {
            Ok(raw) => raw,
            Err(err) => {
                warn!(call_id = %record.call_id, "failed to serialize CDR from database: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "message": format!("Failed to serialize CDR: {err}") })),
                )
                    .into_response();
            }
        };
        let filename = crate::callrecord::default_cdr_file_name(&callrecord);
        (raw, filename)
    };

    let disposition =
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("attachment"));
    let content_type = HeaderValue::from_static("application/json; charset=utf-8");

    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        raw,
    )
        .into_response()
}

/// List recording + signaling artifacts for every CDR leg under a logical
/// `session_id` (root session id). Matches rows whose `session_id` column
/// equals the given id, plus the root row itself (whose `session_id` is
/// NULL or equal to its `call_id` — legacy rows predate the column).
async fn list_session_artifacts(
    AxumPath(session_id): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let mut session_match = Condition::any();
    session_match = session_match.add(CallRecordColumn::SessionId.eq(session_id.clone()));
    session_match = session_match.add(CallRecordColumn::CallId.eq(session_id.clone()));
    let records = match CallRecordEntity::find()
        .filter(session_match)
        .order_by_asc(CallRecordColumn::StartedAt)
        .all(db)
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            warn!(%session_id, %err, "failed to list call records by session_id");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": format!("Failed to query session artifacts: {err}") })),
            )
                .into_response();
        }
    };

    if records.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "message": format!("No call records found for session_id: {session_id}")
            })),
        )
            .into_response();
    }

    Json(json!({
        "session_id": session_id,
        "legs": records.iter().map(session_leg_artifacts).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn session_leg_artifacts(record: &CallRecordModel) -> Value {
    let segments = record
        .metadata
        .as_ref()
        .and_then(|m| m.get("recording_segments"))
        .cloned()
        .unwrap_or(Value::Null);
    let sipflow_jsonl = record
        .metadata
        .as_ref()
        .and_then(|m| m.get("sipflow_jsonl"))
        .cloned()
        .or_else(|| {
            record
                .recording_url
                .as_ref()
                .filter(|u| u.ends_with(".jsonl"))
                .map(|u| Value::String(u.clone()))
        })
        .unwrap_or(Value::Null);
    json!({
        "id": record.id,
        "call_id": record.call_id,
        "leg_role": leg_role(record),
        "direction": record.direction,
        "status": record.status,
        "started_at": record.started_at,
        "ended_at": record.ended_at,
        "recording_url": record.recording_url,
        "recording_segments": segments,
        "sipflow_jsonl": sipflow_jsonl,
        "download_recording": format!("/call-records/{}/recording", record.id),
        "download_sip_flow": format!("/call-records/{}/sip-flow", record.id),
    })
}

/// Derive the logical-call role of a CDR row: the root session's own record
/// (`session_id` NULL or equal to its `call_id`) is the primary; every child
/// leg (queue dispatch, REFER transfer, cluster hop) is a child.
fn leg_role(record: &CallRecordModel) -> &'static str {
    match record.session_id.as_deref() {
        None => "primary",
        Some(session_id) => {
            if session_id == record.call_id {
                "primary"
            } else {
                "child"
            }
        }
    }
}

/// Render an uploaded or local signaling-sidecar JSONL file in the same
/// structured shape as the live backend path.
async fn serve_archived_jsonl_flow(
    record: &CallRecordModel,
    location: &str,
    detail_requested: bool,
    client: &reqwest::Client,
) -> Response {
    let bytes_result: anyhow::Result<Vec<u8>> =
        if location.starts_with("http://") || location.starts_with("https://") {
            match client.get(location).send().await {
                Ok(response) => match response.error_for_status() {
                    Ok(response) => response
                        .bytes()
                        .await
                        .map(|bytes| bytes.to_vec())
                        .map_err(|err| anyhow::Error::from(err.without_url())),
                    Err(err) => Err(anyhow::Error::from(err.without_url())),
                },
                Err(err) => Err(anyhow::Error::from(err.without_url())),
            }
        } else {
            tokio::fs::read(location).await.map_err(anyhow::Error::from)
        };
    let bytes = match bytes_result {
        Ok(bytes) => bytes,
        Err(err) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "message": format!("Archived sipflow JSONL not found: {err}"),
                    "path": location,
                })),
            )
                .into_response();
        }
    };

    let text = String::from_utf8_lossy(&bytes);
    let mut flow: Vec<Value> = Vec::new();
    let mut malformed_lines = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(obj) => {
                let raw_message = obj
                    .get("payload")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let item_call_id = crate::sipflow::storage::extract_callid(raw_message.as_bytes())
                    .unwrap_or_else(|| record.call_id.clone());
                let role = if item_call_id == record.call_id {
                    "caller"
                } else {
                    "callee"
                };
                flow.push(json!({
                    "timestamp": obj.get("timestamp").cloned().unwrap_or(Value::Null),
                    "seq": obj.get("seq").cloned().unwrap_or(Value::Null),
                    "msg_type": obj.get("msg_type").cloned().unwrap_or(Value::Null),
                    "src_addr": obj.get("src_addr").cloned().unwrap_or(Value::Null),
                    "dst_addr": obj.get("dst_addr").cloned().unwrap_or(Value::Null),
                    "raw_message": obj.get("payload").cloned().unwrap_or(Value::Null),
                    "role": role,
                    "call_id": item_call_id,
                }));
            }
            Err(_) => malformed_lines += 1,
        }
    }

    flow.sort_by(|a, b| {
        let a_ts = a.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0);
        let b_ts = b.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0);
        a_ts.partial_cmp(&b_ts).unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_sip_msgs = flow.len();
    let end_time = record.ended_at.unwrap_or(record.started_at);

    Json(json!({
        "call_id": record.call_id,
        "start_time": record.started_at,
        "status": "success",
        "flow": flow,
        "rtp_streams": [],
        "diagnostics": {
            "backend_configured": false,
            "backend_type": "local-jsonl",
            "detail_requested": detail_requested,
            "time_window": {
                "start": record.started_at,
                "end": end_time,
                "base": "jsonl_file",
            },
            "sip_leg_roles_source": "jsonl_payload",
            "cdr_loaded": false,
            "sip_dropped_count": 0,
            "malformed_lines": malformed_lines,
            "total_sip_msgs": total_sip_msgs,
            "total_rtp_streams": 0,
            "legs": [],
        },
    }))
    .into_response()
}

async fn download_call_record_sip_flow(
    AxumPath(identifier): AxumPath<String>,
    Query(query): Query<SipFlowRequestQuery>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let record = match resolve_call_record_by_id_or_call_id(db, &identifier).await {
        Ok(model) => model,
        Err(resp) => return resp,
    };

    // New records persist the exact uploaded JSONL URL. Prefer that archived
    // artifact; older records without it continue through the live-backend
    // query path below.
    if let Some(location) = record
        .metadata
        .as_ref()
        .and_then(|m| m.get("sipflow_jsonl"))
        .and_then(|v| v.as_str())
    {
        let resolved = if location.starts_with("http://")
            || location.starts_with("https://")
            || location.starts_with("s3://")
        {
            if let Some(signed_url) = presign_artifact_url(&state, location).await {
                signed_url
            } else {
                location.to_string()
            }
        } else {
            resolve_archived_artifact_path(location, record.started_at)
        };
        return serve_archived_jsonl_flow(&record, &resolved, query.detail, state.http_client())
            .await;
    }

    let Some(server) = state.sip_server() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "SIP server not available" })),
        )
            .into_response();
    };

    let Some(sipflow) = &server.sip_flow else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "SIP flow not configured" })),
        )
            .into_response();
    };

    let Some(backend) = sipflow.backend() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "SIP flow backend not available" })),
        )
            .into_response();
    };

    let call_time = record.created_at;
    let start_time = (call_time - chrono::Duration::hours(1)).with_timezone(&chrono::Local);
    let end_time = (call_time + chrono::Duration::hours(2)).with_timezone(&chrono::Local);

    let mut call_id_roles: HashMap<String, String> = HashMap::new();
    // Track where each call_id came from, for diagnostics.
    let mut call_id_sources: HashMap<String, &'static str> = HashMap::new();
    // Default main call_id to "primary" if not overridden
    call_id_roles.insert(record.call_id.clone(), "primary".to_string());
    call_id_sources.insert(record.call_id.clone(), "default");

    // Load sip_leg_roles from DB metadata first (faster, no file I/O),
    // then fall back to the CDR JSON file.
    let mut sip_leg_roles_loaded = false;
    let mut sip_leg_roles_source = "default_only";
    if let Some(ref meta) = record.metadata {
        if let Some(meta_map) = meta.as_object() {
            if let Some(json_str) = meta_map.get("sip_leg_roles").and_then(|v| v.as_str()) {
                if let Ok(roles) = serde_json::from_str::<HashMap<String, String>>(json_str) {
                    for (cid, role) in roles {
                        call_id_roles.insert(cid.clone(), role);
                        call_id_sources.insert(cid, "db_metadata");
                    }
                    sip_leg_roles_loaded = true;
                    sip_leg_roles_source = "db_metadata";
                }
            }
        }
    }

    let mut cdr_loaded = false;
    if !sip_leg_roles_loaded {
        let cdr_data = load_cdr_data(&state, &record).await;
        if let Some(cdr) = &cdr_data {
            cdr_loaded = true;
            for (cid, role) in &cdr.record.sip_leg_roles {
                call_id_roles.insert(cid.clone(), role.clone());
                call_id_sources.insert(cid.clone(), "cdr_file");
            }
            sip_leg_roles_source = "cdr_file";
        }
    }

    let mut flow_items = Vec::new();
    let mut rtp_streams = Vec::new();
    let mut legs_diag: Vec<Value> = Vec::new();

    for (cid, role) in &call_id_roles {
        let source = call_id_sources.get(cid).copied().unwrap_or("default");
        let mut sip_msg_count: usize = 0;
        let mut rtp_stream_count: usize = 0;
        let mut leg_error: Option<String> = None;

        if query.detail {
            // Flush first (bounded) so a query issued right after a call
            // ends sees the tail messages still in the write pipeline.
            if query.flush_enabled() {
                crate::callrecord::sipflow::flush_with_deadline(sipflow).await;
            }
            match backend.query_flow(cid, start_time, end_time).await {
                Ok(items) => {
                    sip_msg_count = items.len();
                    for item in items {
                        flow_items.push((item, role.clone(), cid.clone()));
                    }
                }
                Err(err) => {
                    let msg = err.to_string();
                    warn!(identifier = %identifier, call_id = %cid, "failed to query sip flow for leg: {}", err);
                    leg_error = Some(msg);
                }
            }
        }

        match backend.query_media_stats(cid, start_time, end_time).await {
            Ok(stats) => {
                rtp_stream_count = stats.len();
                for stat in stats {
                    let src_addr = if stat.src.is_empty() {
                        format!("Leg {}", stat.leg)
                    } else {
                        stat.src
                    };
                    rtp_streams.push(json!({
                        "role": match stat.leg {
                            0 => "caller",
                            1 => "callee",
                            _ => role.as_str(),
                        },
                        "leg": stat.leg,
                        "src_addr": src_addr,
                        "dst_addr": "RTP",
                        "packet_count": stat.packet_count,
                        "lost_packets": stat.lost_packets,
                        "expected_packets": stat.expected_packets,
                        "loss_percent": stat.loss_percent,
                        "jitter_ms": stat.jitter_ms,
                        "ssrc": stat.ssrc,
                        "ssrc_hex": stat.ssrc.map(|ssrc| format!("0x{ssrc:08x}")),
                        "payload_type": stat.payload_type,
                        "clock_rate": stat.clock_rate,
                    }));
                }
            }
            Err(err) => {
                let msg = err.to_string();
                warn!(identifier = %identifier, call_id = %cid, "failed to query media stats for leg: {}", err);
                leg_error = Some(msg);
            }
        }

        legs_diag.push(json!({
            "call_id": cid,
            "role": role,
            "source": source,
            "sip_msg_count": sip_msg_count,
            "rtp_stream_count": rtp_stream_count,
            "error": leg_error,
        }));
    }

    let total_rtp_streams = rtp_streams.len();

    let mut response = json!({
        "call_id": record.call_id,
        "start_time": record.started_at,
        "status": "success",
        "flow": [],
        "rtp_streams": rtp_streams,
    });

    if query.detail {
        // Sort combined SIP flow by timestamp
        flow_items.sort_by(|(a, _, _), (b, _, _)| {
            a.timestamp
                .partial_cmp(&b.timestamp)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut flow_json = Vec::new();

        for (item, role, cid) in flow_items {
            let raw_message = String::from_utf8_lossy(&item.payload).to_string();
            flow_json.push(json!({
                "timestamp": item.timestamp,
                "seq": item.seq,
                "msg_type": "Sip",
                "src_addr": item.src_addr,
                "dst_addr": item.dst_addr,
                "raw_message": raw_message,
                "role": role,
                "call_id": cid,
            }));
        }

        response["flow"] = Value::Array(flow_json);
    }

    let total_sip_msgs = response["flow"].as_array().map(|f| f.len()).unwrap_or(0);

    response["diagnostics"] = json!({
        "backend_configured": true,
        "backend_type": backend.kind(),
        "detail_requested": query.detail,
        "time_window": {
            "start": start_time.to_rfc3339(),
            "end": end_time.to_rfc3339(),
            "base": "created_at",
            "before_secs": 3600,
            "after_secs": 7200,
        },
        "sip_leg_roles_source": sip_leg_roles_source,
        "cdr_loaded": cdr_loaded,
        "sip_dropped_count": sipflow.dropped_count(),
        "total_sip_msgs": total_sip_msgs,
        "total_rtp_streams": total_rtp_streams,
        "legs": legs_diag,
    });

    Json(response).into_response()
}

async fn stream_call_recording(
    AxumPath(pk): AxumPath<i64>,
    Query(query): Query<RecordingPlaybackQuery>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    headers: HeaderMap,
) -> Response {
    let stream_leg = match parse_recording_stream_selector(query.stream.as_deref()) {
        Ok(selection) => selection,
        Err(message) => {
            return bad_request(
                json!({
                    "message": message,
                    "allowed": ["A", "B", "mixed", "caller", "callee"],
                })
                .to_string(),
            );
        }
    };

    let db = state.db();
    let record = find_or_404!(CallRecordEntity, pk, db, "Call record");

    let cdr_data = load_cdr_data(&state, &record).await;
    // A `?segment=` selector pins the playback to one recording segment of a
    // segmented call (unique_id / track_id). A missing match must 404 rather
    // than silently falling back to another segment or the whole-call
    // SipFlow rendition.
    let requested_segment = query
        .segment
        .as_deref()
        .map(str::trim)
        .filter(|selector| !selector.is_empty());
    let recording_path = match requested_segment {
        Some(selector) => {
            let found = select_recording_segment_path(&record, cdr_data.as_ref(), Some(selector));
            if found.is_none() {
                return (
                    StatusCode::NOT_FOUND,
                    [(http::header::CACHE_CONTROL, "no-store")],
                    Json(json!({
                        "message": "Recording segment not found",
                        "segment": selector,
                    })),
                )
                    .into_response();
            }
            found
        }
        None => select_recording_path(&record, cdr_data.as_ref()),
    };

    // Try to stream from file first
    if let Some(ref path) = recording_path
        && let Ok(meta) = tokio::fs::metadata(path).await
        && meta.is_file()
        && meta.len() > 0
    {
        // Handle stream selection (A=caller/left, B=callee/right, mixed=full stereo)
        // for file-based recordings (non-sipflow).
        if let Some(stream) = stream_leg
            && let Ok(channel_data) = extract_channel_from_wav(path, stream)
        {
            return Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "audio/wav")
                .header(http::header::CONTENT_LENGTH, channel_data.len())
                .header("x-available-streams", "A,B,mixed")
                .header(
                    http::header::CONTENT_DISPOSITION,
                    "inline; filename=\"recording.wav\"",
                )
                .body(Body::from(channel_data))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        // Fall through for mixed stream, non-WAV files, or extraction failure
        return stream_file_with_range(path, meta.len(), &headers).await;
    }

    // Fallback: Try to get recording from sipflow backend
    if let Some(server) = state.sip_server()
        && let Some(sipflow) = &server.sip_flow
        && let Some(backend) = sipflow.backend()
    {
        let call_time = record.created_at;
        let start_time = (call_time - chrono::Duration::hours(1)).with_timezone(&chrono::Local);
        let end_time = (call_time + chrono::Duration::hours(2)).with_timezone(&chrono::Local);

        crate::callrecord::sipflow::flush_with_deadline(sipflow).await;

        let wav_result: Result<tempfile::NamedTempFile, _> = backend
            .generate_wav_file(&record.call_id, start_time, end_time, stream_leg)
            .await;

        if let Ok(temp_file) = wav_result {
            let temp_path = temp_file.path().to_owned();
            let file_len = match tokio::fs::metadata(&temp_path).await {
                Ok(m) => m.len(),
                Err(_) => 0,
            };

            if file_len <= 44 {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(http::header::CACHE_CONTROL, "no-store")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "message": "Recording is not ready" }).to_string(),
                    ))
                    .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response());
            }

            let mut header_buf = [0u8; 4];
            if let Ok(mut f) = tokio::fs::File::open(&temp_path).await {
                let _ = f.seek(std::io::SeekFrom::Start(40)).await;
                let _ = f.read_exact(&mut header_buf).await;
            }
            let data_size = u32::from_le_bytes(header_buf);
            if record.duration_secs > 1 && data_size <= 1280 {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(http::header::CACHE_CONTROL, "no-store")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "message": "Recording is not ready" }).to_string(),
                    ))
                    .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response());
            }

            let tmp_path = temp_file.into_temp_path();
            let path_str = tmp_path.to_string_lossy().to_string();
            let response = stream_file_with_range(&path_str, file_len, &headers).await;
            let _ = tokio::fs::remove_file(&path_str).await;
            drop(tmp_path);
            return response;
        }
    }

    (
        StatusCode::NOT_FOUND,
        [(http::header::CACHE_CONTROL, "no-store")],
        Json(json!({ "message": "Recording not found" })),
    )
        .into_response()
}

async fn stream_file_with_range(
    recording_path: &str,
    file_len: u64,
    headers: &HeaderMap,
) -> Response {
    let range_header = headers
        .get(http::header::RANGE)
        .and_then(|value| value.to_str().ok());
    let (status, start, end) =
        match range_header.and_then(|value| parse_range_header(value, file_len)) {
            Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end),
            None if range_header.is_some() => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                response.headers_mut().insert(
                    http::header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{}", file_len))
                        .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
                );
                return response;
            }
            _ => (StatusCode::OK, 0, file_len.saturating_sub(1)),
        };

    let mut file = match tokio::fs::File::open(&recording_path).await {
        Ok(file) => file,
        Err(err) => {
            warn!(path = %recording_path, "failed to open recording file: {}", err);
            return internal_error(format!("Failed to open recording file: {}", err));
        }
    };

    if start > 0
        && let Err(err) = file.seek(std::io::SeekFrom::Start(start)).await
    {
        warn!(path = %recording_path, "failed to seek recording file: {}", err);
        return internal_error(format!("Failed to read recording file: {}", err));
    }

    let bytes_to_send = end.saturating_sub(start) + 1;
    let stream = ReaderStream::new(file.take(bytes_to_send));

    let body = Body::from_stream(stream);
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers_mut = response.headers_mut();
    headers_mut.insert(
        http::header::ACCEPT_RANGES,
        HeaderValue::from_static("bytes"),
    );
    headers_mut.insert(
        http::header::CONTENT_LENGTH,
        HeaderValue::from_str(&bytes_to_send.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );

    if status == StatusCode::PARTIAL_CONTENT {
        headers_mut.insert(
            http::header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, file_len))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
        );
    }

    let file_name = Path::new(&recording_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recording");
    let mime = guess_audio_mime(file_name);
    let safe_file_name = crate::utils::sanitize_header_value(file_name);
    if let Ok(mime_value) = HeaderValue::from_str(mime) {
        headers_mut.insert(http::header::CONTENT_TYPE, mime_value);
    }

    if let Ok(disposition) =
        HeaderValue::from_str(&format!("inline; filename=\"{}\"", safe_file_name))
    {
        headers_mut.insert(http::header::CONTENT_DISPOSITION, disposition);
    }

    response
}

fn parse_range_header(range: &str, file_len: u64) -> Option<(u64, u64)> {
    let value = range.strip_prefix("bytes=")?;
    let range_value = value.split(',').next()?.trim();
    if range_value.is_empty() {
        return None;
    }

    let mut parts = range_value.splitn(2, '-');
    let start_part = parts.next().unwrap_or("");
    let end_part = parts.next().unwrap_or("");

    if start_part.is_empty() {
        let suffix_len = end_part.parse::<u64>().ok()?;
        if suffix_len == 0 {
            return None;
        }
        if suffix_len >= file_len {
            return Some((0, file_len.saturating_sub(1)));
        }
        let start_pos = file_len - suffix_len;
        return Some((start_pos, file_len.saturating_sub(1)));
    }

    let start_pos = start_part.parse::<u64>().ok()?;
    if start_pos >= file_len {
        return None;
    }

    if end_part.is_empty() {
        return Some((start_pos, file_len.saturating_sub(1)));
    }

    let end_pos = end_part.parse::<u64>().ok()?;
    if end_pos < start_pos || end_pos >= file_len {
        return None;
    }

    Some((start_pos, end_pos))
}

async fn page_call_records(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let filters = match load_filters(state.db()).await {
        Ok(filters) => filters,
        Err(err) => {
            warn!("failed to load call record filters: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": format!("Failed to load call records: {}", err) })),
            )
                .into_response();
        }
    };

    let current_user = state.build_current_user_ctx(&user).await;

    state.render_with_headers(
        "console/call_records.html",
        json!({
            "nav_active": "call-records",
            "base_path": state.base_path(),
            "filter_options": filters,
            "list_url": state.url_for("/call-records"),
            "page_size_options": vec![10, 25, 50],
            "current_user": current_user,
        }),
        &headers,
    )
}

async fn query_call_records(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::ListQuery<QueryCallRecordFilters>>,
) -> Response {
    let filters = payload.filters.clone();
    // Determine CDR date from filter date_from (fallback to today).
    let cdr_date = filters.as_ref().and_then(|f| f.date_from.as_deref());
    let cdb = state.cdr_db(cdr_date).await;

    let rdb = state.db();
    let condition = build_condition(&filters);

    let mut selector = CallRecordEntity::find().filter(condition.clone());

    let sort_key = payload.sort.as_deref().unwrap_or("started_at_desc");
    match sort_key {
        "started_at_asc" => {
            selector = selector.order_by(CallRecordColumn::StartedAt, Order::Asc);
        }
        "duration_desc" => {
            selector = selector.order_by(CallRecordColumn::DurationSecs, Order::Desc);
        }
        "duration_asc" => {
            selector = selector.order_by(CallRecordColumn::DurationSecs, Order::Asc);
        }
        _ => {
            selector = selector.order_by(CallRecordColumn::StartedAt, Order::Desc);
        }
    }
    selector = selector.order_by(CallRecordColumn::Id, Order::Desc);

    let paginator = selector.paginate(&cdb, payload.normalize().1);
    let pagination = match forms::paginate(paginator, &payload).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to paginate call records: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let related = match load_related_context(&rdb, &pagination.items).await {
        Ok(related) => related,
        Err(err) => {
            warn!("failed to load related data for call records: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let mut inline_recordings: Vec<Option<String>> = Vec::with_capacity(pagination.items.len());
    for record in &pagination.items {
        let has_recording_url = record
            .recording_url
            .as_ref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if has_recording_url {
            inline_recordings.push(None);
            continue;
        }

        let inline_url = match load_cdr_data(&state, record).await {
            Some(cdr_data) => select_recording_path(record, Some(&cdr_data))
                .map(|_| state.url_for(&format!("/call-records/{}/recording", record.id))),
            None => None,
        };

        inline_recordings.push(inline_url);
    }

    let mut items: Vec<Value> = Vec::with_capacity(pagination.items.len());
    for (record, inline) in pagination.items.iter().zip(inline_recordings.iter()) {
        items.push(build_record_payload(record, &related, &state, inline.as_deref()).await);
    }

    let summary = match build_summary(&cdb, condition).await {
        Ok(summary) => summary,
        Err(err) => {
            warn!("failed to build call record summary: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    Json(json!({
        "page": pagination.current_page,
        "per_page": pagination.per_page,
        "total_pages": pagination.total_pages,
        "total_items": pagination.total_items,
        "items": items,
        "summary": summary,
    }))
    .into_response()
}

async fn page_call_record_detail(
    AxumPath(id_param): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let db = state.db();
    let model_result = if let Ok(pk) = id_param.parse::<i64>() {
        CallRecordEntity::find_by_id(pk).one(db).await
    } else {
        CallRecordEntity::find()
            .filter(CallRecordColumn::CallId.eq(&id_param))
            .one(db)
            .await
    };

    let model = match model_result {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load call record '{}': {}", id_param, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let related = match load_related_context(db, std::slice::from_ref(&model)).await {
        Ok(related) => related,
        Err(err) => {
            warn!(
                "failed to load related data for call record '{}': {}",
                id_param, err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let cdr_data = load_cdr_data(&state, &model).await;

    // Sibling CDR legs of the same logical call: everything whose
    // `session_id` matches this record's logical call, excluding the record
    // itself. Resolving by `session_id` (falling back to `call_id` for root
    // rows) keeps queue-dispatch / REFER-transfer / cluster-hop legs visible
    // on the detail page of the primary CDR.
    let session_key = model
        .session_id
        .clone()
        .unwrap_or_else(|| model.call_id.clone());
    let child_legs = match CallRecordEntity::find()
        .filter(CallRecordColumn::SessionId.eq(session_key.clone()))
        .filter(CallRecordColumn::CallId.ne(model.call_id.clone()))
        .order_by_asc(CallRecordColumn::StartedAt)
        .all(db)
        .await
    {
        Ok(legs) => legs,
        Err(err) => {
            warn!(
                "failed to load child legs for call record '{}': {}",
                id_param, err
            );
            Vec::new()
        }
    };

    let payload =
        build_detail_payload(&model, &related, &state, cdr_data.as_ref(), &child_legs).await;
    let current_user = state.build_current_user_ctx(&user).await;

    state.render_with_headers(
        "console/call_record_detail.html",
        json!({
            "nav_active": "call-records",
            "page_title": format!("Call record · {}", model.id),
            "call_id": model.call_id,
            "call_data": serde_json::to_string(&payload).unwrap_or_default(),

            "addon_scripts": state.get_injected_scripts(&format!("{}/call-records/{}", state.base_path(), model.id)),
            "current_user": current_user,
        }),
        &headers,
    )
}

async fn update_call_record(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    Json(payload): Json<UpdateCallRecordPayload>,
) -> Response {
    if payload.tags.is_none() && payload.note.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "No updates supplied" })),
        )
            .into_response();
    }

    let db = state.db();
    let mut record =
        crate::console::config_helpers::find_or_404!(CallRecordEntity, pk, db, "Call record");

    let mut active: CallRecordActiveModel = record.clone().into();
    let mut changed = false;

    if let Some(tags_input) = payload.tags.as_ref() {
        let normalized = normalize_string_list(Some(tags_input));
        let new_tags_value = if normalized.is_empty() {
            None
        } else {
            Some(Value::Array(
                normalized
                    .iter()
                    .map(|value| Value::String(value.clone()))
                    .collect(),
            ))
        };
        let tags_changed = match (&record.tags, &new_tags_value) {
            (None, None) => false,
            (Some(old), Some(new)) => old != new,
            _ => true,
        };
        if tags_changed {
            active.tags = Set(new_tags_value.clone());
            record.tags = new_tags_value;
            changed = true;
        }
    }

    let notes_payload = if let Some(note) = &payload.note {
        let new_text = note.text.as_deref().unwrap_or("").to_string();
        let existing_text = record
            .metadata
            .as_ref()
            .and_then(|m| m.get("call_notes"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if new_text != existing_text {
            let mut merged = match record.metadata.take() {
                Some(Value::Object(map)) => map,
                _ => serde_json::Map::new(),
            };
            merged.insert("call_notes".into(), Value::String(new_text.clone()));
            active.metadata = Set(Some(Value::Object(merged)));
            changed = true;
        }
        Some(json!({
            "text": new_text,
            "updated_at": Utc::now(),
            "updated_by": Value::Null,
        }))
    } else {
        None
    };

    if !changed {
        let response = json!({
            "status": "noop",
            "record": {
                "id": record.id,
                "tags": extract_tags(&record.tags),
            },
            "notes": notes_payload.unwrap_or(Value::Null),
        });
        return Json(response).into_response();
    }

    active.updated_at = Set(Utc::now());
    let updated_record = match active.update(db).await {
        Ok(model) => model,
        Err(err) => {
            warn!(call_record_id = pk, "failed to update call record: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": format!("Failed to update call record: {}", err) })),
            )
                .into_response();
        }
    };

    let response = json!({
        "status": "ok",
        "record": {
            "id": updated_record.id,
            "tags": extract_tags(&updated_record.tags),
        },
        "notes": notes_payload.unwrap_or(Value::Null),
    });
    Json(response).into_response()
}

async fn delete_call_record(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
) -> Response {
    if let Err(resp) = state.require_permission(&user, "cdr", "delete").await {
        return resp;
    }
    match CallRecordEntity::delete_by_id(pk).exec(state.db()).await {
        Ok(result) => {
            if result.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "message": "Call record not found" })),
                )
                    .into_response()
            } else {
                StatusCode::NO_CONTENT.into_response()
            }
        }
        Err(err) => {
            warn!("failed to delete call record '{}': {}", pk, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response()
        }
    }
}

async fn load_filters(db: &DatabaseConnection) -> Result<Value, DbErr> {
    let departments: Vec<Value> = DepartmentEntity::find()
        .order_by_asc(DepartmentColumn::Name)
        .all(db)
        .await?
        .into_iter()
        .map(|dept| json!({ "id": dept.id, "name": dept.name }))
        .collect();

    let sip_trunks: Vec<Value> = SipTrunkEntity::find()
        .order_by_asc(SipTrunkColumn::Name)
        .all(db)
        .await?
        .into_iter()
        .map(|trunk| {
            json!({
                "id": trunk.id,
                "name": trunk.name,
                "display_name": trunk.display_name,
            })
        })
        .collect();

    Ok(json!({
        "status": ["any", "completed", "missed", "failed"],
        "direction": ["any", "inbound", "outbound", "internal"],
        "departments": departments,
        "sip_trunks": sip_trunks,
        "tags": [],
    }))
}

fn build_condition(filters: &Option<QueryCallRecordFilters>) -> Condition {
    use sea_orm::sea_query::{Expr, ExprTrait};
    let mut condition = Condition::all();

    // Logical-call view (default): only root-session CDRs, so a transferred
    // call lists once. `all_legs` opts into per-leg rows. `session_id` equals
    // `call_id` on the root row and is NULL on legacy rows.
    let include_all_legs = filters.as_ref().and_then(|f| f.all_legs).unwrap_or(false);
    if !include_all_legs {
        let mut primary_only = Condition::any();
        primary_only = primary_only.add(CallRecordColumn::SessionId.is_null());
        primary_only = primary_only.add(
            Expr::col((CallRecordEntity, CallRecordColumn::SessionId))
                .equals((CallRecordEntity, CallRecordColumn::CallId)),
        );
        condition = condition.add(primary_only);
    }

    if let Some(filters) = filters {
        if let Some(q_raw) = filters.q.as_ref() {
            let trimmed = q_raw.trim();
            if !trimmed.is_empty() {
                let mut q_condition = Condition::any();
                q_condition = q_condition.add(CallRecordColumn::CallId.eq(trimmed));
                q_condition = q_condition.add(CallRecordColumn::ToNumber.eq(trimmed));
                q_condition = q_condition.add(CallRecordColumn::FromNumber.eq(trimmed));
                condition = condition.add(q_condition);
            }
        }

        if let Some(status_raw) = filters.status.as_ref() {
            let status_trimmed = status_raw.trim();
            if !status_trimmed.is_empty() && !equals_ignore_ascii_case(status_trimmed, "any") {
                condition = condition.add(CallRecordColumn::Status.eq(status_trimmed));
            }
        }

        if let Some(direction_raw) = filters.direction.as_ref() {
            let direction_trimmed = direction_raw.trim();
            if !direction_trimmed.is_empty() && !equals_ignore_ascii_case(direction_trimmed, "any")
            {
                condition = condition.add(CallRecordColumn::Direction.eq(direction_trimmed));
            }
        }

        if let Some(caller_raw) = filters.caller.as_ref() {
            let caller_trimmed = caller_raw.trim();
            if !caller_trimmed.is_empty() {
                let pattern = format!("%{}%", caller_trimmed);
                condition = condition.add(CallRecordColumn::FromNumber.like(pattern));
            }
        }

        if let Some(callee_raw) = filters.callee.as_ref() {
            let callee_trimmed = callee_raw.trim();
            if !callee_trimmed.is_empty() {
                let pattern = format!("%{}%", callee_trimmed);
                condition = condition.add(CallRecordColumn::ToNumber.like(pattern));
            }
        }

        let date_from = parse_date(filters.date_from.as_ref(), false);
        let date_to = parse_date(filters.date_to.as_ref(), true);

        if let Some(from) = date_from {
            condition = condition.add(CallRecordColumn::StartedAt.gte(from));
        } else if filters.q.is_some() {
            // Default to 30 days if searching without date range to prevent full table scan
            let thirty_days_ago = Utc::now() - chrono::Duration::days(30);
            condition = condition.add(CallRecordColumn::StartedAt.gte(thirty_days_ago));
        }

        if let Some(to) = date_to {
            condition = condition.add(CallRecordColumn::StartedAt.lte(to));
        }

        if filters.only_transcribed.unwrap_or(false) {
            condition = condition.add(CallRecordColumn::HasTranscript.eq(true));
        }

        let department_ids = normalize_i64_list(filters.department_ids.as_ref());
        if !department_ids.is_empty() {
            condition = condition.add(CallRecordColumn::DepartmentId.is_in(department_ids));
        }

        let sip_trunk_ids = normalize_i64_list(filters.sip_trunk_ids.as_ref());
        if !sip_trunk_ids.is_empty() {
            condition = condition.add(CallRecordColumn::SipTrunkId.is_in(sip_trunk_ids));
        }

        let outbound_sip_trunk_ids = normalize_i64_list(filters.outbound_sip_trunk_ids.as_ref());
        if !outbound_sip_trunk_ids.is_empty() {
            condition =
                condition.add(CallRecordColumn::OutboundSipTrunkId.is_in(outbound_sip_trunk_ids));
        }

        let tags = normalize_string_list(filters.tags.as_ref());
        if !tags.is_empty() {
            let mut any_tag = Condition::any();
            for tag in tags {
                let escaped = tag.replace('"', "\\\"");
                let pattern = format!("%\"{}\"%", escaped);
                any_tag = any_tag.add(CallRecordColumn::Tags.like(pattern));
            }
            condition = condition.add(any_tag);
        }
    }

    condition
}

fn parse_date(raw: Option<&String>, end_of_day: bool) -> Option<DateTime<Utc>> {
    let value = raw?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let date = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok()?;
    let naive = if end_of_day {
        date.and_hms_opt(23, 59, 59)?
    } else {
        date.and_hms_opt(0, 0, 0)?
    };
    Utc.from_local_datetime(&naive).single()
}

fn normalize_i64_list(input: Option<&Vec<i64>>) -> Vec<i64> {
    let mut values = input.cloned().unwrap_or_default();
    values.sort_unstable();
    values.dedup();
    values
}

fn normalize_string_list(input: Option<&Vec<String>>) -> Vec<String> {
    let mut values: Vec<String> = input
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    values.sort();
    values.dedup();
    values
}

async fn load_related_context(
    db: &DatabaseConnection,
    records: &[CallRecordModel],
) -> Result<RelatedContext, DbErr> {
    let mut extension_ids = HashSet::new();
    let mut department_ids = HashSet::new();
    let mut sip_trunk_ids = HashSet::new();

    for record in records {
        if let Some(id) = record.extension_id {
            extension_ids.insert(id);
        }
        if let Some(id) = record.department_id {
            department_ids.insert(id);
        }
        if let Some(id) = record.sip_trunk_id {
            sip_trunk_ids.insert(id);
        }
        if let Some(id) = record.outbound_sip_trunk_id {
            sip_trunk_ids.insert(id); // Using the same sip_trunk_ids pool for query
        }
    }

    let extensions = if extension_ids.is_empty() {
        HashMap::new()
    } else {
        let ids: Vec<i64> = extension_ids.into_iter().collect();
        ExtensionEntity::find()
            .filter(crate::models::extension::Column::Id.is_in(ids.clone()))
            .all(db)
            .await?
            .into_iter()
            .map(|model| (model.id, model))
            .collect()
    };

    let departments = if department_ids.is_empty() {
        HashMap::new()
    } else {
        let ids: Vec<i64> = department_ids.into_iter().collect();
        DepartmentEntity::find()
            .filter(DepartmentColumn::Id.is_in(ids.clone()))
            .all(db)
            .await?
            .into_iter()
            .map(|model| (model.id, model))
            .collect()
    };

    let sip_trunks = if sip_trunk_ids.is_empty() {
        HashMap::new()
    } else {
        let ids: Vec<i64> = sip_trunk_ids.into_iter().collect();
        SipTrunkEntity::find()
            .filter(SipTrunkColumn::Id.is_in(ids.clone()))
            .all(db)
            .await?
            .into_iter()
            .map(|model| (model.id, model))
            .collect()
    };

    Ok(RelatedContext {
        extensions,
        departments,
        sip_trunks,
    })
}

struct RelatedContext {
    extensions: HashMap<i64, ExtensionModel>,
    departments: HashMap<i64, DepartmentModel>,
    sip_trunks: HashMap<i64, SipTrunkModel>,
}

fn resolve_cdr_storage(state: &ConsoleState) -> Option<CdrStorage> {
    let app = state.app_state()?;
    match storage::resolve_storage(app.config().callrecord.as_ref()) {
        Ok(storage) => storage,
        Err(err) => {
            warn!("failed to resolve call record storage: {}", err);
            None
        }
    }
}

pub struct CdrData {
    pub record: CallRecord,
    pub raw_content: String,
    pub cdr_path: String,
    pub storage: Option<CdrStorage>,
}

async fn build_record_payload(
    record: &CallRecordModel,
    related: &RelatedContext,
    state: &ConsoleState,
    inline_recording_url: Option<&str>,
) -> Value {
    let tags = extract_tags(&record.tags);
    let extension_number = record
        .extension_id
        .and_then(|id| related.extensions.get(&id))
        .map(|ext| ext.extension.clone());
    let department_name = record
        .department_id
        .and_then(|id| related.departments.get(&id))
        .map(|dept| dept.name.clone());
    let sip_trunk_name = record
        .sip_trunk_id
        .and_then(|id| related.sip_trunks.get(&id))
        .map(|trunk| {
            trunk
                .display_name
                .clone()
                .unwrap_or_else(|| trunk.name.clone())
        });
    let sip_gateway = record
        .sip_gateway
        .clone()
        .or_else(|| sip_trunk_name.clone());
    let outbound_trunk_name = record
        .outbound_sip_trunk_id
        .and_then(|id| related.sip_trunks.get(&id))
        .map(|trunk| trunk.display_name.clone().unwrap_or(trunk.name.clone()))
        .or_else(|| metadata_string(record.metadata.as_ref(), OUTBOUND_TRUNK_NAME_KEY));

    let outbound_trunk_dest = metadata_string(record.metadata.as_ref(), OUTBOUND_TRUNK_DEST_KEY);

    // The route rule matched during routing. `route_id` references
    // rustpbx_routes (None for config-file rules); the name comes from the
    // CDR metadata written by the reporter.
    let route_name = metadata_string(record.metadata.as_ref(), "route_name");

    let caller_uri = record.caller_uri.clone();
    let callee_uri = record.callee_uri.clone();

    let recording = build_recording_payload(state, record, inline_recording_url).await;
    let error = build_error_payload(record.metadata.as_ref());

    let rewrite_caller_original = record.rewrite_original_from.clone();
    let rewrite_caller_final = caller_uri.clone();
    let rewrite_callee_original = record.rewrite_original_to.clone();
    let rewrite_callee_final = callee_uri.clone();
    let rewrite_contact = Option::<String>::None;
    let rewrite_destination = Option::<String>::None;
    let status_code = Option::<u16>::None;
    let ring_time = metadata_string(record.metadata.as_ref(), "ring_time");
    let answer_time = metadata_string(record.metadata.as_ref(), "answer_time");
    let hangup_reason = Option::<String>::None;
    let hangup_messages = Vec::<Value>::new();

    json!({
        "id": record.id,
        "call_id": record.call_id,
        "session_id": record.session_id,
        "leg_role": leg_role(record),
        "display_id": record.display_id,
        "direction": record.direction,
        "status": record.status,
        "from": record.from_number,
        "to": record.to_number,
        "caller_name": record.caller_name,
        "agent": record.agent_name,
        "agent_extension": extension_number,
        "department": department_name,
        "queue": record.queue,
        "caller_uri": caller_uri,
        "callee_uri": callee_uri,
        "sip_gateway": sip_gateway,
        "sip_trunk": sip_trunk_name,
        "outbound_trunk": outbound_trunk_name,
        "outbound_trunk_id": record.outbound_sip_trunk_id,
        "outbound_trunk_dest": outbound_trunk_dest,
        "route_id": record.route_id,
        "route_name": route_name,
        "self_ip": metadata_string(record.metadata.as_ref(), "self_ip"),
        "hostname": metadata_string(record.metadata.as_ref(), "hostname"),
        "tags": tags,
        "has_transcript": record.has_transcript,
        "transcript_status": record.transcript_status,
        "transcript_language": record.transcript_language,
        "duration_secs": record.duration_secs,
        "recording": recording,
        "started_at": record.started_at.to_rfc3339(),
        "ring_time": ring_time,
        "answer_time": answer_time,
        "ended_at": record.ended_at.map(|dt| dt.to_rfc3339()),
        "detail_url": state.url_for(&format!("/call-records/{}", record.id)),
        "status_code": status_code,
        "hangup_reason": hangup_reason,
        "hangup_messages": hangup_messages,
        "error": error,
        "rewrite": {
            "caller": {
                "original": rewrite_caller_original,
                "final": rewrite_caller_final,
            },
            "callee": {
                "original": rewrite_callee_original,
                "final": rewrite_callee_final,
            },
            "contact": rewrite_contact,
            "destination": rewrite_destination,
        },
    })
}

fn metadata_string(metadata: Option<&Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|value| value.as_object())
        .and_then(|meta| meta.get(key))
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Build the structured error payload for the record UI chip. Reads the/// standardized error fields written by the reporter into `metadata`, and
/// augments them with the catalog's `locale_key` / `remediation_key` (resolved
/// against the live [`CallErrRegistry`]) so templates can localize via `| t`.
///
/// Returns `Null` when no `error_code` is present (the template hides the chip
/// with `x-if`).
fn build_error_payload(metadata: Option<&Value>) -> Value {
    let Some(code) = metadata_string(metadata, "error_code") else {
        return Value::Null;
    };
    let app = metadata_string(metadata, "error_app");
    let severity = metadata_string(metadata, "error_severity");
    let message = metadata_string(metadata, "error_message");
    // Augment with catalog locale/remediation keys when the code is registered.
    let (locale_key, remediation_key) = match crate::call_errors::registry().find(&code) {
        Some(info) => (Some(info.locale_key), info.remediation_key),
        None => (None, None),
    };
    json!({
        "code": code,
        "app": app,
        "severity": severity,
        "message": message,
        "locale_key": locale_key,
        "remediation_key": remediation_key,
    })
}

/// Extract the call trace timeline from `metadata["trace"]` (a JSON array of
/// [`crate::call_errors::TraceEvent`] objects). Returns the array, or `Null`
/// when absent. Only surfaced on the detail page trace tab.
fn build_trace_payload(metadata: Option<&Value>) -> Value {
    metadata
        .and_then(|value| value.as_object())
        .and_then(|meta| meta.get("trace"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Lifetime applied to presigned URLs when the owning config section is
/// absent (should not happen in practice; kept as a safe fallback).
const FALLBACK_SIGNED_URL_EXPIRY_SECS: u64 = 86_400;

/// Rewrite a stored OSS/S3 object URL (`{endpoint}/{bucket}/{key}` or
/// `s3://{bucket}/{key}`) into a presigned download URL, so recordings and SIP-flow
/// remain accessible when the bucket is private. The signature is generated
/// on demand from the upload storage credentials and stays valid for the
/// configured `signed_url_expiry_secs` window (capped at 7 days by SigV4).
///
/// Returns `None` when no configured storage owns the URL or signing fails.
async fn presign_artifact_url(state: &ConsoleState, raw_url: &str) -> Option<String> {
    let app = state.app_state()?;
    let core = &app.core;

    // Resolve live storages + expiries from the late-bound upload runtimes so
    // hot-reloaded `[recording]` / `[sipflow.upload]` configs are honored.
    // Recording candidates cover the default bucket AND every
    // `[recording.sources.*]` per-source bucket, each presigned with its own
    // credentials.
    let mut candidates: Vec<(Storage, Duration)> = Vec::new();
    if let Some(recording) = core
        .recording_upload
        .as_ref()
        .and_then(|rt| rt.resolve_targets().ok().flatten())
    {
        let expiry = Duration::from_secs(recording.0.effective_signed_url_expiry_secs());
        for target in recording.1.s3_targets() {
            candidates.push((target.storage.clone(), expiry));
        }
    }
    if let Some(sipflow) = core.sipflow_upload.as_ref().and_then(|rt| {
        rt.resolve()
            .ok()
            .flatten()
            .and_then(|(policy, storage)| storage.map(|s| (s, policy.signed_url_expiry_secs())))
    }) {
        candidates.push((
            sipflow.0,
            Duration::from_secs(sipflow.1.unwrap_or(FALLBACK_SIGNED_URL_EXPIRY_SECS)),
        ));
    }

    for (storage, expiry_secs) in candidates.iter() {
        if !storage.supports_presign() {
            continue;
        }
        let Some(key) = storage.object_key_from_url(raw_url) else {
            continue;
        };
        return match storage.presign_read_url(&key, *expiry_secs).await {
            Ok(signed) => Some(signed),
            Err(err) => {
                debug!(%err, "failed to presign artifact URL");
                None
            }
        };
    }
    None
}

async fn build_recording_payload(
    state: &ConsoleState,
    record: &CallRecordModel,
    inline_recording_url: Option<&str>,
) -> Option<Value> {
    let raw = record
        .recording_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());

    let (url, supports_streams) = if let Some(raw_value) = raw {
        if raw_value.starts_with("http://") || raw_value.starts_with("https://") {
            // External URL – presign OSS/S3 objects when possible
            (
                presign_artifact_url(state, raw_value)
                    .await
                    .unwrap_or_else(|| raw_value.to_string()),
                false,
            )
        } else {
            // Local file path – serve through /recording endpoint with stream selection
            (
                state.url_for(&format!("/call-records/{}/recording", record.id)),
                true,
            )
        }
    } else if let Some(fallback) = inline_recording_url {
        // CDR data with recording file on disk – serve through /recording endpoint
        (fallback.to_string(), true)
    } else if state
        .sip_server()
        .map(|server| server.sip_flow.is_some())
        .unwrap_or(false)
    {
        // If sipflow is enabled, allow trying to play/download the recording
        (
            state.url_for(&format!("/call-records/{}/recording", record.id)),
            true,
        )
    } else {
        return None;
    };

    Some(json!({
        "url": url,
        "duration_secs": record.recording_duration_secs,
        "supports_streams": supports_streams,
    }))
}

async fn derive_recording_download_url(
    state: &ConsoleState,
    record: &CallRecordModel,
) -> Option<String> {
    if let Some(raw) = record
        .recording_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return Some(
                presign_artifact_url(state, raw)
                    .await
                    .unwrap_or_else(|| raw.to_string()),
            );
        } else {
            return Some(state.url_for(&format!("/call-records/{}/recording", record.id)));
        }
    }

    if state
        .sip_server()
        .map(|server| server.sip_flow.is_some())
        .unwrap_or(false)
    {
        return Some(state.url_for(&format!("/call-records/{}/recording", record.id)));
    }

    None
}

/// Per-segment playback list for segmented calls, sourced from the CDR
/// `recorder` media list (one entry per recording segment plus the
/// `signaling` sidecar). Returns `None` for calls with fewer than two
/// segments so single-file calls keep the existing payload shape.
///
/// Each entry's `playback_url` targets that segment only: the local
/// `/call-records/{id}/recording?segment={unique_id}` endpoint when the file
/// is on disk, otherwise the presigned per-segment upload URL when present.
async fn build_recording_segments_payload(
    state: &ConsoleState,
    record: &CallRecordModel,
    cdr: Option<&CdrData>,
) -> Option<Value> {
    let cdr_data = cdr?;
    let entries: Vec<&crate::callrecord::CallRecordMedia> = cdr_data
        .record
        .recorder
        .iter()
        .filter(|media| media.track_id != "signaling" && !media.path.trim().is_empty())
        .collect();
    if entries.len() < 2 {
        return None;
    }

    let mut segments = Vec::with_capacity(entries.len());
    for media in entries {
        let extra = media.extra.as_ref();
        let string_extra = |key: &str| {
            extra
                .and_then(|bag| bag.get(key))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let number_extra = |key: &str| {
            extra
                .and_then(|bag| bag.get(key))
                .and_then(|value| value.as_u64())
        };
        let duration_secs = match (string_extra("started_at"), string_extra("ended_at")) {
            (Some(started), Some(ended)) => chrono::DateTime::parse_from_rfc3339(&started)
                .ok()
                .zip(chrono::DateTime::parse_from_rfc3339(&ended).ok())
                .map(|(started, ended)| (ended - started).num_seconds().max(0)),
            _ => None,
        };
        // Selector used by `?segment=`: unique_id preferred (matches
        // `recording_metadata_available`), track_id fallback.
        let selector = media
            .unique_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or(&media.track_id);
        // Local files are served through the console endpoint, which also
        // accepts `?stream=`; remote objects are served by their presigned
        // upload URL, whose query string must stay untouched (signature).
        let local_playback = select_recording_segment_path(record, cdr, Some(selector)).is_some();
        let playback_url = if local_playback {
            Some(state.url_for(&format!(
                "/call-records/{}/recording?segment={}",
                record.id,
                urlencoding::encode(selector)
            )))
        } else {
            match extra
                .and_then(|bag| bag.get("uploadUrl"))
                .and_then(|value| value.as_str().map(str::trim).filter(|s| !s.is_empty()))
            {
                Some(upload_url)
                    if upload_url.starts_with("http://") || upload_url.starts_with("https://") =>
                {
                    Some(
                        presign_artifact_url(state, upload_url)
                            .await
                            .unwrap_or_else(|| upload_url.to_string()),
                    )
                }
                _ => None,
            }
        };
        segments.push(json!({
            "unique_id": media.unique_id,
            "track_id": media.track_id,
            "segment_type": string_extra("segment_type"),
            "segment_id": string_extra("segment_id"),
            "seq": number_extra("seq"),
            "label": string_extra("label"),
            "started_at": string_extra("started_at"),
            "ended_at": string_extra("ended_at"),
            "duration_secs": duration_secs,
            "size": media.size,
            "supports_streams": local_playback,
            "playback_url": playback_url,
        }));
    }
    Some(Value::Array(segments))
}

fn strip_storage_root(state: &ConsoleState, path: &str) -> String {
    if let Some(app) = state.app_state() {
        if let Some(config) = &app.config().callrecord {
            match &config.storage {
                crate::config::CallRecordStorageConfig::Local { root } => {
                    let root_path = Path::new(root);
                    let candidate_path = Path::new(path);
                    if let Ok(stripped) = candidate_path.strip_prefix(root_path) {
                        stripped.to_string_lossy().to_string()
                    } else {
                        let root_str = root.trim_end_matches('/');
                        if let Some(stripped) = path.strip_prefix(root_str) {
                            stripped.trim_start_matches('/').to_string()
                        } else {
                            path.to_string()
                        }
                    }
                }
                crate::config::CallRecordStorageConfig::S3 { root, .. } => {
                    let root_str = root.trim_end_matches('/');
                    if let Some(stripped) = path.strip_prefix(root_str) {
                        stripped.trim_start_matches('/').to_string()
                    } else {
                        path.to_string()
                    }
                }
                _ => path.to_string(),
            }
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

/// Read the raw CDR JSON content for a record, trying the path persisted at
/// write time first and then the path reconstructed from the current storage
/// root. Returns `(raw_content, resolved_path, storage)`.
async fn read_cdr_raw(
    state: &ConsoleState,
    record: &CallRecordModel,
) -> Option<(String, String, Option<CdrStorage>)> {
    let app = state.app_state()?;
    let root = match app
        .config()
        .callrecord
        .as_ref()
        .map(|config| &config.storage)
    {
        Some(crate::config::CallRecordStorageConfig::Local { root })
        | Some(crate::config::CallRecordStorageConfig::S3 { root, .. }) => root.as_str(),
        _ => "",
    };
    let storage = resolve_cdr_storage(state);
    let callrecord: CallRecord = record.clone().into();
    let reconstructed = crate::callrecord::format_file_name(root, &callrecord);

    // Candidate full-paths to try, in priority order:
    // 1. The path persisted at write time (survives storage root changes — #237).
    // 2. The path reconstructed from the current root (legacy / fallback).
    let mut candidates: Vec<String> = Vec::new();
    if let Some(stored) = record
        .metadata
        .as_ref()
        .and_then(|m| m.get("cdr_path"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        if !candidates.iter().any(|c| c == stored) {
            candidates.push(stored.to_string());
        }
    }
    candidates.push(reconstructed.clone());

    let is_local = storage.as_ref().map(|s| s.is_local()).unwrap_or(false);

    let mut content: Option<String> = None;
    let mut resolved_path: Option<String> = None;

    for candidate in &candidates {
        if content.is_some() {
            break;
        }

        // For local storage, try a direct filesystem read first. This recovers
        // historical CDRs whose file still lives under a previous storage root
        // (the absolute persisted path) after the operator changed the root —
        // see issue #237.
        if is_local && Path::new(candidate).is_absolute() {
            if let Ok(value) = tokio::fs::read_to_string(candidate).await {
                content = Some(value);
                resolved_path = Some(candidate.clone());
                continue;
            }
        }

        // Storage-backed read: strip the current root, then read via storage.
        if let Some(ref storage_ref) = storage {
            let path_to_read = strip_storage_root(state, candidate);
            match storage_ref.read_to_string(&path_to_read).await {
                Ok(value) => {
                    content = Some(value);
                    resolved_path = Some(candidate.clone());
                }
                Err(err) => {
                    warn!(call_id = %record.call_id, path = %path_to_read, "failed to load CDR from storage: {}", err);
                }
            }
        }
    }

    let cdr_path = resolved_path.unwrap_or(reconstructed);

    content.map(|raw| (raw, cdr_path, storage))
}

pub async fn load_cdr_data(state: &ConsoleState, record: &CallRecordModel) -> Option<CdrData> {
    let (raw, cdr_path, storage) = read_cdr_raw(state, record).await?;
    match serde_json::from_str::<CallRecord>(&raw) {
        Ok(parsed) => {
            return Some(CdrData {
                record: parsed,
                raw_content: raw,
                cdr_path,
                storage,
            });
        }
        Err(err) => {
            warn!(call_id = %record.call_id, path = %cdr_path, "failed to parse CDR file: {}", err);
        }
    }

    None
}

fn guess_audio_mime(file_name: &str) -> &'static str {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    match ext.as_deref() {
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("ogg") | Some("oga") | Some("opus") => "audio/ogg",
        Some("flac") => "audio/flac",
        _ => "application/octet-stream",
    }
}

pub fn select_recording_path(record: &CallRecordModel, cdr: Option<&CdrData>) -> Option<String> {
    if let Some(cdr_data) = cdr {
        for media in &cdr_data.record.recorder {
            let path = media.path.trim();
            if path.is_empty() {
                continue;
            }
            let resolved = resolve_archived_artifact_path(path, record.started_at);
            if Path::new(&resolved).exists() {
                return Some(resolved);
            }
        }
    }

    if let Some(url) = record.recording_url.as_ref() {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            let resolved = resolve_archived_artifact_path(trimmed, record.started_at);
            if Path::new(&resolved).exists() {
                return Some(resolved);
            }
        }
    }

    None
}

/// Resolve the media entry matching a `?segment=` selector (`unique_id`
/// first, `track_id` fallback) and return its on-disk path. Returns `None`
/// when the selector is empty, no entry matches, or the file is gone — the
/// caller turns that into a 404 instead of silently playing another segment.
pub fn select_recording_segment_path(
    record: &CallRecordModel,
    cdr: Option<&CdrData>,
    segment: Option<&str>,
) -> Option<String> {
    let selector = segment.map(str::trim).filter(|s| !s.is_empty())?;
    let cdr_data = cdr?;
    for media in &cdr_data.record.recorder {
        let matches = media
            .unique_id
            .as_deref()
            .map(|id| id == selector)
            .unwrap_or(false)
            || media.track_id == selector;
        if !matches {
            continue;
        }
        let path = media.path.trim();
        if path.is_empty() {
            continue;
        }
        let resolved = resolve_archived_artifact_path(path, record.started_at);
        if Path::new(&resolved).exists() {
            return Some(resolved);
        }
    }
    None
}

/// Extract a single channel from a stereo WAV file and return it as a mono WAV.
/// Returns `Ok(data)` on success, `Err` if the file is not a valid 16-bit stereo WAV.
/// The caller should fall back to serving the full file on error.
fn extract_channel_from_wav(path: &str, stream_leg: i32) -> anyhow::Result<Vec<u8>> {
    let mut reader = WavReader::open(path)?;
    let spec = *reader.spec();

    if spec.channels < 2 || spec.bits_per_sample != 16 {
        anyhow::bail!("not a 16-bit stereo WAV");
    }

    let channel_idx = match stream_leg {
        0 => 0usize,
        1 => 1usize,
        _ => anyhow::bail!("invalid stream_leg: expected 0 or 1"),
    };

    let all_samples: Vec<i16> = reader
        .samples()
        .collect::<::std::result::Result<Vec<_>, _>>()?;

    let channel_samples: Vec<i16> = all_samples
        .chunks(spec.channels as usize)
        .map(|chunk| chunk[channel_idx])
        .collect();

    let mut buf = Vec::new();
    let mono_spec = WavSpec {
        channels: 1,
        sample_rate: spec.sample_rate,
        bits_per_sample: spec.bits_per_sample,
        sample_format: spec.sample_format,
    };
    let mut writer = WavWriter::new(std::io::Cursor::new(&mut buf), mono_spec)?;
    for sample in &channel_samples {
        writer.write_sample(*sample)?;
    }
    writer.finalize()?;

    Ok(buf)
}

async fn build_detail_payload(
    record: &CallRecordModel,
    related: &RelatedContext,
    state: &ConsoleState,
    cdr: Option<&CdrData>,
    child_legs: &[CallRecordModel],
) -> Value {
    let inline_recording_url = select_recording_path(record, cdr)
        .map(|_| state.url_for(&format!("/call-records/{}/recording", record.id)));
    let mut record_payload =
        build_record_payload(record, related, state, inline_recording_url.as_deref()).await;
    // Segmented calls: attach the per-segment playback list so the detail
    // page can offer a segment selector. Single-file calls keep the existing
    // payload shape (no `segments` key).
    if let Some(segments) = build_recording_segments_payload(state, record, cdr).await {
        if let Some(recording) = record_payload
            .get_mut("recording")
            .and_then(|value| value.as_object_mut())
        {
            recording.insert("segments".to_string(), segments);
        }
    }
    let participants = build_participants(record, related);

    // Per-leg media quality captured by the MediaBridge at call end (RTCP
    // jitter/RTT/loss + packet counters), stored in metadata["media_quality"].
    let media_metrics = record
        .metadata
        .as_ref()
        .and_then(|m| m.get("media_quality"))
        .cloned()
        .unwrap_or(Value::Null);

    let signaling = if let Some(data) = cdr {
        build_signaling_from_cdr(data)
    } else {
        Value::Null
    };

    let mut rewrite = record_payload
        .get("rewrite")
        .cloned()
        .unwrap_or(Value::Null);

    if let Some(data) = cdr {
        let details_rewrite = &data.record.details.rewrite;
        rewrite = json!({
            "caller": {
                "original": details_rewrite.caller_original,
                "final": details_rewrite.caller_final,
            },
            "callee": {
                "original": details_rewrite.callee_original,
                "final": details_rewrite.callee_final,
            },
            "contact": details_rewrite.contact,
            "destination": details_rewrite.destination,
        });
    }

    let sip_flow_download =
        state.url_for(&format!("/call-records/{}/sip-flow?detail=true", record.id));
    let cdr_json_download = state.url_for(&format!("/call-records/{}/cdr-json", record.id));

    let mut download_recording = derive_recording_download_url(state, record).await;
    if download_recording.is_none() {
        download_recording = inline_recording_url.clone();
    }

    let notes = record
        .metadata
        .as_ref()
        .and_then(|m| m.get("call_notes"))
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .map(|text| {
            json!({
                "text": text,
                "updated_at": Value::Null,
                "updated_by": Value::Null,
            })
        });

    json!({
        "back_url": state.url_for("/call-records"),
        "error_codes_url": state.url_for("/error-codes"),
        "record": record_payload,
        "trace": build_trace_payload(record.metadata.as_ref()),
        //"sip_flow": sip_flow_download,
        "media_metrics": media_metrics,
        "notes": notes.unwrap_or(Value::Null),
        "participants": participants,
        "signaling": signaling,
        "rewrite": rewrite,
        // Sibling CDR legs of the same logical call (queue dispatch, REFER
        // transfer, cluster hops). Empty for single-leg calls.
        "child_legs": child_legs
            .iter()
            .map(|leg| {
                json!({
                    "id": leg.id,
                    "call_id": leg.call_id,
                    "leg_role": leg_role(leg),
                    "direction": leg.direction,
                    "status": leg.status,
                    "from": leg.from_number,
                    "to": leg.to_number,
                    "agent": leg.agent_name,
                    "queue": leg.queue,
                    "duration_secs": leg.duration_secs,
                    "started_at": leg.started_at.to_rfc3339(),
                    "detail_url": state.url_for(&format!("/call-records/{}", leg.id)),
                })
            })
            .collect::<Vec<_>>(),
        "actions": json!({
            "download_recording": download_recording,
            "download_sip_flow": sip_flow_download,
            "download_cdr_json": cdr_json_download,
            "transcript_url": state.api_url_for(&format!("/call-records/{}/transcript", record.id)),
            "update_record": state.api_url_for(&format!("/call-records/{}", record.id)),
        }),
    })
}

fn build_signaling_from_cdr(cdr: &CdrData) -> Value {
    let mut legs = Vec::new();
    append_cdr_leg(&mut legs, "primary", &cdr.record);
    if legs.is_empty() {
        return Value::Null;
    }
    json!({
        "is_b2bua": false,
        "legs": legs,
    })
}

fn append_cdr_leg(legs: &mut Vec<Value>, role: &str, record: &CallRecord) {
    legs.push(signaling_leg_payload(role, record));
}

fn signaling_leg_payload(role: &str, record: &CallRecord) -> Value {
    json!({
        "role": role,
        "call_id": record.call_id,
        "caller": record.caller,
        "callee": record.callee,
        "status_code": record.status_code,
        "hangup_reason": record
            .hangup_reason
            .as_ref()
            .map(|reason| reason.to_string()),
        "hangup_messages": record.hangup_messages,
        "last_error": record.details.last_error,
        "start_time": record.start_time,
        "ring_time": record.ring_time,
        "answer_time": record.answer_time,
        "end_time": record.end_time,
    })
}

fn build_participants(record: &CallRecordModel, related: &RelatedContext) -> Value {
    let extension_number = record
        .extension_id
        .and_then(|id| related.extensions.get(&id))
        .map(|ext| ext.extension.clone());

    let gateway_label = record
        .sip_gateway
        .clone()
        .unwrap_or_else(|| "External".to_string());

    let mut participants = Vec::new();

    participants.push(json!({
        "role": "caller",
        "label": "Caller",
        "name": record
            .caller_name
            .clone()
            .or_else(|| record.caller_uri.clone()),
        "number": record.from_number.clone(),
        "uri": record.caller_uri.clone(),
        "network": gateway_label.clone(),
    }));

    if record.callee_uri.is_some() || record.to_number.is_some() || record.agent_name.is_some() {
        let callee_name = record
            .callee_uri
            .clone()
            .or_else(|| record.to_number.clone())
            .or_else(|| record.agent_name.clone());
        let remote_network = record
            .sip_gateway
            .clone()
            .unwrap_or_else(|| "Remote".to_string());
        participants.push(json!({
            "role": "callee",
            "label": "Callee",
            "name": callee_name,
            "number": record.to_number.clone(),
            "uri": record.callee_uri.clone(),
            "network": remote_network,
        }));
    }

    if record.agent_name.is_some() || extension_number.is_some() {
        participants.push(json!({
            "role": "agent",
            "label": "Agent",
            "name": record.agent_name.clone(),
            "number": extension_number.clone(),
            "uri": extension_number.clone(),
            "network": "PBX",
        }));
    }

    Value::Array(participants)
}

fn extract_tags(tags: &Option<Value>) -> Vec<String> {
    match tags {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|value| value.as_str())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Aggregate summary counts for the call record list (applies the same
/// filter condition as the list itself).
async fn build_summary(db: &DatabaseConnection, condition: Condition) -> Result<Value, DbErr> {
    use sea_orm::sea_query::{Expr, Func, IntoCondition, SimpleExpr};
    use sea_orm::{DatabaseBackend, ExprTrait, QuerySelect};

    fn count_when<C: IntoCondition>(cond: C) -> SimpleExpr {
        crate::utils::count_when(cond)
    }

    let sum_cast = match db.get_database_backend() {
        DatabaseBackend::Sqlite => "INTEGER",
        DatabaseBackend::MySql => "SIGNED",
        DatabaseBackend::Postgres => "BIGINT",
        _ => "BIGINT",
    };
    // MySQL 5.7 CAST() has no DOUBLE/REAL/FLOAT, and sqlx cannot decode DECIMAL
    // as f64. Promoting AVG(...) with + 0E0 yields a DOUBLE.
    let avg_expr = match db.get_database_backend() {
        DatabaseBackend::Sqlite => {
            SimpleExpr::from(Func::avg(Expr::col(CallRecordColumn::DurationSecs))).cast_as("REAL")
        }
        DatabaseBackend::MySql => Expr::cust_with_expr(
            "? + 0E0",
            Func::avg(Expr::col(CallRecordColumn::DurationSecs)),
        ),
        _ => {
            SimpleExpr::from(Func::avg(Expr::col(CallRecordColumn::DurationSecs))).cast_as("FLOAT8")
        }
    };

    #[derive(sea_orm::FromQueryResult)]
    struct SummaryAgg {
        total: i64,
        answered: i64,
        missed: i64,
        failed: i64,
        inbound: i64,
        outbound: i64,
        transcribed: i64,
        total_secs: Option<i64>,
        avg_secs: Option<f64>,
        unique_dids: i64,
    }

    let agg = CallRecordEntity::find()
        .filter(condition.clone())
        .select_only()
        .column_as(CallRecordColumn::Id.count(), "total")
        .column_as(
            count_when(CallRecordColumn::Status.eq("completed")),
            "answered",
        )
        .column_as(count_when(CallRecordColumn::Status.eq("missed")), "missed")
        .column_as(count_when(CallRecordColumn::Status.eq("failed")), "failed")
        .column_as(
            count_when(CallRecordColumn::Direction.eq("inbound")),
            "inbound",
        )
        .column_as(
            count_when(CallRecordColumn::Direction.eq("outbound")),
            "outbound",
        )
        .column_as(
            count_when(CallRecordColumn::HasTranscript.eq(true)),
            "transcribed",
        )
        .column_as(
            SimpleExpr::from(Func::sum(Expr::col(CallRecordColumn::DurationSecs)))
                .cast_as(sum_cast),
            "total_secs",
        )
        .column_as(avg_expr, "avg_secs")
        .column_as(
            Expr::col(CallRecordColumn::FromNumber).count_distinct(),
            "unique_dids",
        )
        .into_model::<SummaryAgg>()
        .one(db)
        .await?;

    let Some(agg) = agg else {
        return Ok(json!({
            "total": 0, "answered": 0, "missed": 0, "failed": 0,
            "transcribed": 0, "avg_duration": 0.0, "total_minutes": 0.0,
            "inbound": 0, "outbound": 0, "asr": 0.0, "unique_dids": 0,
        }));
    };

    let answered = std::cmp::max(agg.answered, 0) as u64;
    let total = std::cmp::max(agg.total, 0) as u64;
    let asr = if total > 0 {
        (answered as f64 / total as f64) * 100.0
    } else {
        0.0
    };
    let total_minutes = std::cmp::max(agg.total_secs.unwrap_or(0), 0) as f64 / 60.0;

    Ok(json!({
        "total": agg.total,
        "answered": agg.answered,
        "missed": agg.missed,
        "failed": agg.failed,
        "transcribed": agg.transcribed,
        "avg_duration": agg.avg_secs.unwrap_or(0.0).max(0.0),
        "total_minutes": total_minutes,
        "inbound": agg.inbound,
        "outbound": agg.outbound,
        "asr": asr,
        "unique_dids": agg.unique_dids,
    }))
}

fn equals_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn parse_recording_stream_selector(stream: Option<&str>) -> Result<Option<i32>, &'static str> {
    match stream.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Some(0)),
        Some(value) if equals_ignore_ascii_case(value, "a") => Ok(Some(0)),
        Some(value) if equals_ignore_ascii_case(value, "caller") => Ok(Some(0)),
        Some(value) if equals_ignore_ascii_case(value, "b") => Ok(Some(1)),
        Some(value) if equals_ignore_ascii_case(value, "callee") => Ok(Some(1)),
        Some(value) if equals_ignore_ascii_case(value, "mixed") => Ok(None),
        Some(_) => Err("Invalid stream selector"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::handlers::test_helpers::{superuser, unprivileged_user};
    use crate::media::wav_reader::SampleFormat;
    use crate::{
        config::ConsoleConfig,
        console::{ConsoleState, middleware::AuthRequired},
        models::{call_record, migration::Migrator, routing},
    };
    use axum::{Router, extract::State, http::StatusCode, routing::get};
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use std::sync::Arc;

    async fn setup_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        Migrator::up(&db, None).await.expect("migrations succeed");
        db
    }

    async fn create_console_state(db: DatabaseConnection) -> Arc<ConsoleState> {
        ConsoleState::initialize(db, ConsoleConfig::default(), None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn private_artifacts_are_retrieved_using_signed_urls() {
        let temp = tempfile::tempdir().unwrap();
        let wav_path = temp.path().join("fixture.wav");
        create_test_stereo_wav(wav_path.to_str().unwrap());
        let wav = tokio::fs::read(&wav_path).await.unwrap();
        let served_wav = wav.clone();
        let mock = Router::new().route("/{*key}", get(move |headers: HeaderMap, uri: http::Uri| {
            let wav = served_wav.clone();
            async move {
                if headers.contains_key(http::header::AUTHORIZATION)
                    || !uri.query().unwrap_or_default().contains("X-Amz-Signature=")
                    || uri.query().unwrap_or_default().contains("stream=") {
                    return StatusCode::FORBIDDEN.into_response();
                }
                if uri.path().ends_with(".wav") {
                    ([(http::header::CONTENT_TYPE, "audio/wav")], wav).into_response()
                } else {
                    "{\"timestamp\":1,\"seq\":0,\"msg_type\":\"Sip\",\"payload\":\"INVITE sip:b SIP/2.0\\r\\n\"}\n".into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let mock_task = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
        let config = crate::config::Config {
            database_url: "sqlite::memory:".into(),
            storage: Some(crate::storage::StorageConfig::Local {
                path: temp.path().join("storage").to_string_lossy().into_owned(),
            }),
            recording: Some(crate::config::RecordingPolicy {
                enabled: Some(true),
                recording_type: Some(crate::config::RecordingType::S3),
                vendor: Some(crate::storage::S3Vendor::Aliyun),
                endpoint: Some(endpoint.clone()),
                access_key: Some("test".into()),
                secret_key: Some("test".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let app = crate::app::AppStateBuilder::new()
            .with_config(config)
            .with_skip_sip_bind()
            .build()
            .await
            .unwrap();
        let state = create_console_state(app.db().clone()).await;
        state.set_app_state(Some(Arc::downgrade(&app)));
        let record = call_record::ActiveModel {
            call_id: Set("private-artifacts".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            duration_secs: Set(10),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            recording_url: Set(Some(format!("{endpoint}/recordings/call.wav"))),
            metadata: Set(Some(
                json!({"sipflow_jsonl": format!("{endpoint}/sipflow/call.jsonl")}),
            )),
            ..Default::default()
        }
        .insert(app.db())
        .await
        .unwrap();
        let payload = build_recording_payload(&state, &record, None)
            .await
            .unwrap();
        let signed = payload["url"].as_str().unwrap();
        assert!(signed.starts_with(&format!("{endpoint}/recordings/call.wav?")));
        assert_eq!(payload["supports_streams"], false);
        let response = state.http_client().get(signed).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), wav.as_slice());
        let location = format!("{endpoint}/sipflow/call.jsonl");
        let signed = presign_artifact_url(&state, &location).await.unwrap();
        let response = serve_archived_jsonl_flow(&record, &signed, true, state.http_client()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["flow"].as_array().unwrap().len(), 1);
        app.token().cancel();
        mock_task.abort();
    }

    #[tokio::test]
    async fn load_filters_returns_defaults() {
        let db = setup_db().await;
        let filters = load_filters(&db).await.expect("filters");
        assert!(filters.get("status").is_some());
        assert!(filters.get("direction").is_some());
    }

    #[test]
    fn parse_recording_stream_selector_defaults_to_a_leg() {
        assert_eq!(parse_recording_stream_selector(None).unwrap(), Some(0));
        assert_eq!(parse_recording_stream_selector(Some(" ")).unwrap(), Some(0));
    }

    #[test]
    fn parse_recording_stream_selector_accepts_aliases() {
        assert_eq!(parse_recording_stream_selector(Some("A")).unwrap(), Some(0));
        assert_eq!(
            parse_recording_stream_selector(Some("caller")).unwrap(),
            Some(0)
        );
        assert_eq!(parse_recording_stream_selector(Some("B")).unwrap(), Some(1));
        assert_eq!(
            parse_recording_stream_selector(Some("callee")).unwrap(),
            Some(1)
        );
        assert_eq!(
            parse_recording_stream_selector(Some("mixed")).unwrap(),
            None
        );
    }

    #[test]
    fn parse_recording_stream_selector_rejects_unknown_values() {
        assert!(parse_recording_stream_selector(Some("foobar")).is_err());
    }

    async fn insert_summary_fixture(db: &DatabaseConnection) {
        for (call_id, duration_secs) in [("summary-call-1", 60), ("summary-call-2", 30)] {
            call_record::ActiveModel {
                call_id: Set(call_id.into()),
                direction: Set("inbound".into()),
                status: Set("completed".into()),
                started_at: Set(Utc::now()),
                duration_secs: Set(duration_secs),
                has_transcript: Set(false),
                transcript_status: Set("pending".into()),
                created_at: Set(Utc::now()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(db)
            .await
            .expect("insert call record");
        }
    }

    async fn assert_summary_aggregates(db: &DatabaseConnection) {
        let summary = build_summary(db, Condition::all())
            .await
            .expect("build summary");

        assert_eq!(summary["total"], 2);
        assert_eq!(summary["avg_duration"], 45.0);
        assert_eq!(summary["total_minutes"], 1.5);
    }

    #[tokio::test]
    async fn session_leg_artifacts_includes_segments_and_sipflow_jsonl() {
        let db = setup_db().await;
        let model = call_record::ActiveModel {
            call_id: Set("leg-a".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            recording_url: Set(Some("/rec/root.wav".into())),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            metadata: Set(Some(json!({
                "recording_segments": [{"path":"/rec/root_ts_ivr_1.wav","segmentType":"ivr"}],
                "sipflow_jsonl": "/rec/root_leg-a.jsonl"
            }))),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert");

        let artifacts = session_leg_artifacts(&model);
        assert_eq!(artifacts["call_id"], "leg-a");
        assert!(artifacts["recording_segments"].is_array());
        assert_eq!(artifacts["sipflow_jsonl"], "/rec/root_leg-a.jsonl");
        assert!(
            artifacts["download_recording"]
                .as_str()
                .unwrap()
                .contains(&model.id.to_string())
        );
    }

    /// The local-JSONL fallback must render the same structured JSON shape as
    /// the sipflow backend path (flow/rtp_streams/diagnostics), never a raw
    /// JSONL download, so the console UI can always consume `response.json()`.
    #[tokio::test]
    async fn serve_local_jsonl_flow_returns_structured_json() {
        let db = setup_db().await;
        let model = call_record::ActiveModel {
            call_id: Set("jsonl-call".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert");

        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl = dir.path().join("jsonl-call.jsonl");
        let later_ts = chrono::Utc::now().timestamp_micros() as u64;
        let earlier_ts = later_ts - 5_000;
        std::fs::write(
            &jsonl,
            format!(
                "{{\"timestamp\":{later_ts},\"seq\":1,\"msg_type\":\"Sip\",\"src_addr\":\"\",\"dst_addr\":\"10.0.0.1:5060\",\"payload\":\"SIP/2.0 200 OK\\r\\nCall-ID: jsonl-call\\r\\n\"}}\n\
                 {{\"timestamp\":{earlier_ts},\"seq\":0,\"msg_type\":\"Sip\",\"src_addr\":\"10.0.0.1:5060\",\"dst_addr\":\"\",\"payload\":\"INVITE sip:b@x SIP/2.0\\r\\nCall-ID: callee-jsonl-call\\r\\n\"}}\n\
                 not-json\n"
            ),
        )
        .expect("write jsonl");

        let client = crate::http_util::build_keepalive_client(None, None).unwrap();
        let response =
            serve_archived_jsonl_flow(&model, jsonl.to_str().unwrap(), true, &client).await;
        let (parts, body) = response.into_parts();
        assert_eq!(parts.status, StatusCode::OK);
        assert!(
            parts
                .headers
                .get(http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["status"], "success");
        assert_eq!(value["call_id"], "jsonl-call");
        let flow = value["flow"].as_array().unwrap();
        assert_eq!(flow.len(), 2);
        // Sorted by timestamp ascending regardless of file line order.
        assert_eq!(flow[0]["seq"], 0);
        assert_eq!(flow[0]["role"], "callee");
        assert_eq!(flow[0]["call_id"], "callee-jsonl-call");
        assert_eq!(flow[1]["role"], "caller");
        assert_eq!(flow[1]["call_id"], "jsonl-call");
        assert_eq!(flow[1]["dst_addr"], "10.0.0.1:5060");
        let diag = &value["diagnostics"];
        assert_eq!(diag["backend_type"], "local-jsonl");
        assert_eq!(diag["backend_configured"], false);
        assert_eq!(diag["total_sip_msgs"], 2);
        assert_eq!(diag["total_rtp_streams"], 0);
        assert_eq!(diag["malformed_lines"], 1);
        assert_eq!(diag["sip_leg_roles_source"], "jsonl_payload");
        assert!(value["rtp_streams"].as_array().unwrap().is_empty());

        // Missing file yields 404, still JSON.
        let missing = dir.path().join("nope.jsonl");
        let response =
            serve_archived_jsonl_flow(&model, missing.to_str().unwrap(), true, &client).await;
        let (parts, _) = response.into_parts();
        assert_eq!(parts.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_archived_jsonl_flow_reads_http_url() {
        let db = setup_db().await;
        let model = call_record::ActiveModel {
            call_id: Set("remote-jsonl-call".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert");

        let app = Router::new().route(
            "/flow.jsonl",
            get(|| async {
                "{\"timestamp\":1,\"seq\":0,\"msg_type\":\"Sip\",\"src_addr\":\"a\",\"dst_addr\":\"b\",\"payload\":\"INVITE sip:b SIP/2.0\\r\\n\"}\n"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind JSONL server");
        let address = listener.local_addr().expect("JSONL server address");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let client = crate::http_util::build_keepalive_client(None, None).unwrap();
        let response = serve_archived_jsonl_flow(
            &model,
            &format!("http://{address}/flow.jsonl"),
            true,
            &client,
        )
        .await;
        let (parts, body) = response.into_parts();
        assert_eq!(parts.status, StatusCode::OK);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["flow"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_session_artifacts_matches_root_leg_by_call_id() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;
        for call_id in ["leg-1", "leg-2"] {
            call_record::ActiveModel {
                call_id: Set(call_id.into()),
                direction: Set("inbound".into()),
                status: Set("completed".into()),
                started_at: Set(Utc::now()),
                duration_secs: Set(5),
                has_transcript: Set(false),
                transcript_status: Set("pending".into()),
                metadata: Set(Some(
                    json!({"sipflow_jsonl": format!("/rec/{call_id}.jsonl")}),
                )),
                created_at: Set(Utc::now()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            }
            .insert(&db)
            .await
            .expect("insert");
        }

        let response = list_session_artifacts(
            AxumPath("leg-1".into()),
            State(state),
            AuthRequired(superuser()),
        )
        .await;
        let (parts, body) = response.into_parts();
        assert_eq!(parts.status, StatusCode::OK);
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["session_id"], "leg-1");
        assert_eq!(value["legs"].as_array().unwrap().len(), 1);
        assert_eq!(value["legs"][0]["call_id"], "leg-1");
    }

    #[tokio::test]
    async fn build_summary_aggregates_durations() {
        let db = setup_db().await;
        insert_summary_fixture(&db).await;
        assert_summary_aggregates(&db).await;
    }

    #[tokio::test]
    async fn build_summary_aggregates_durations_on_mysql57() {
        // Example: mysql://root:pass@127.0.0.1:13357/rustpbx_avg_cast_test?ssl-mode=DISABLED
        let Ok(url) = std::env::var("MYSQL57_URL") else {
            eprintln!("skipping: MYSQL57_URL not set");
            return;
        };
        if url.trim().is_empty() {
            eprintln!("skipping: MYSQL57_URL empty");
            return;
        }

        let db = Database::connect(url.trim())
            .await
            .expect("connect mysql 5.7");
        Migrator::up(&db, None)
            .await
            .expect("mysql 5.7 migrations succeed");
        call_record::Entity::delete_many()
            .filter(CallRecordColumn::CallId.is_in(["summary-call-1", "summary-call-2"]))
            .exec(&db)
            .await
            .expect("cleanup previous mysql fixture");
        insert_summary_fixture(&db).await;
        assert_summary_aggregates(&db).await;
    }

    #[tokio::test]
    async fn build_record_payload_contains_basic_fields() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;
        let ring_time = Utc::now();
        let answer_time = ring_time + chrono::Duration::seconds(5);

        let record = call_record::ActiveModel {
            call_id: Set("call-1".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(60),
            metadata: Set(Some(json!({
                "ring_time": ring_time.to_rfc3339(),
                "answer_time": answer_time.to_rfc3339(),
            }))),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let related = load_related_context(&db, &[record.clone()])
            .await
            .expect("related context");
        let payload = build_record_payload(&record, &related, &state, None).await;
        assert_eq!(payload["id"], 1);
        assert_eq!(payload["ring_time"], ring_time.to_rfc3339());
        assert_eq!(payload["answer_time"], answer_time.to_rfc3339());
    }

    #[tokio::test]
    async fn build_record_payload_includes_outbound_trunk_metadata() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("call-outbound-meta-1".into()),
            direction: Set("outbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(60),
            metadata: Set(Some(json!({
                "outbound_trunk_name": "carrier-a",
                "outbound_trunk_dest": "sip:carrier-a.example.com:5060"
            }))),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let related = load_related_context(&db, &[record.clone()])
            .await
            .expect("related context");
        let payload = build_record_payload(&record, &related, &state, None).await;
        assert_eq!(payload["outbound_trunk"], "carrier-a");
        assert_eq!(
            payload["outbound_trunk_dest"],
            "sip:carrier-a.example.com:5060"
        );
    }

    #[tokio::test]
    #[cfg(feature = "addon-wholesale")]
    async fn build_record_payload_exposes_error_from_metadata() {
        // Empirically proves the call-record UI error chip data path: the
        // standardized error fields written by the reporter into the metadata
        // JSON column are surfaced as a structured `error` object for both the
        // list and detail templates.
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("call-err-1".into()),
            direction: Set("inbound".into()),
            status: Set("failed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(0),
            metadata: Set(Some(json!({
                "error_code": "wholesale.insufficient_funds",
                "error_app": "wholesale",
                "error_severity": "error",
                "error_message": "Insufficient funds",
                "sip_code": "402",
            }))),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let related = load_related_context(&db, &[record.clone()])
            .await
            .expect("related context");
        let payload = build_record_payload(&record, &related, &state, None).await;
        assert_eq!(payload["error"]["code"], "wholesale.insufficient_funds");
        assert_eq!(payload["error"]["app"], "wholesale");
        assert_eq!(payload["error"]["severity"], "error");
        assert_eq!(payload["error"]["message"], "Insufficient funds");
        // registered code -> catalog locale/remediation keys are attached
        assert_eq!(
            payload["error"]["locale_key"],
            "errors.wholesale.insufficient_funds"
        );
        assert!(payload["error"]["remediation_key"].is_string());
    }

    #[tokio::test]
    async fn build_record_payload_error_null_when_no_error_code() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;
        let record = call_record::ActiveModel {
            call_id: Set("call-ok-1".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(30),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");
        let related = load_related_context(&db, &[record.clone()])
            .await
            .expect("related context");
        let payload = build_record_payload(&record, &related, &state, None).await;
        assert!(payload["error"].is_null());
        assert!(payload["ring_time"].is_null());
        assert!(payload["answer_time"].is_null());
    }

    #[tokio::test]
    async fn build_record_payload_exposes_matched_route() {
        // The matched route surfaces for the UI: route_id from the FK column,
        // route_name from the reporter-written metadata (also covering
        // config-file rules without a database id).
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let route = routing::ActiveModel {
            name: Set("us-outbound".into()),
            direction: Set(routing::RoutingDirection::Outbound),
            priority: Set(100),
            is_active: Set(true),
            selection_strategy: Set(routing::RoutingSelectionStrategy::default()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert route");

        let record = call_record::ActiveModel {
            call_id: Set("call-route-1".into()),
            direction: Set("outbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            route_id: Set(Some(route.id)),
            metadata: Set(Some(json!({
                "route_name": "us-outbound"
            }))),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let related = load_related_context(&db, &[record.clone()])
            .await
            .expect("related context");
        let payload = build_record_payload(&record, &related, &state, None).await;
        assert_eq!(payload["route_id"], route.id);
        assert_eq!(payload["route_name"], "us-outbound");
    }

    #[tokio::test]
    async fn build_trace_payload_extracts_trace_array() {
        // The call trace (a JSON array under metadata["trace"]) must be surfaced
        // for the detail page trace tab.
        let db = setup_db().await;
        let _state = create_console_state(db.clone()).await;
        let record = call_record::ActiveModel {
            call_id: Set("call-trace-1".into()),
            direction: Set("inbound".into()),
            status: Set("failed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(0),
            metadata: Set(Some(json!({
                "trace": [
                    { "ts": 0, "kind": "ring", "message": "Dialing", "severity": "info" },
                    { "ts": 2500, "kind": "answer", "message": "Call answered", "severity": "info" },
                    { "ts": 5200, "kind": "play", "message": "Played prompt", "duration_ms": 1200, "interrupted": false, "severity": "info" },
                    { "ts": 8000, "kind": "end", "message": "Call ended", "code": "ivr.timeout", "severity": "warn" }
                ]
            }))),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let trace = build_trace_payload(record.metadata.as_ref());
        let arr = trace.as_array().expect("trace is an array");
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["kind"], "ring");
        assert_eq!(arr[3]["kind"], "end");
        assert_eq!(arr[3]["severity"], "warn");
        // No trace -> Null
        let no_trace = build_trace_payload(Some(&json!({ "error_code": "proxy.callee_offline" })));
        assert!(no_trace.is_null());
    }

    #[tokio::test]
    async fn delete_call_record_denied_without_permission() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;
        let record = call_record::ActiveModel {
            call_id: Set("del-denied-1".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let user = unprivileged_user();
        let resp = delete_call_record(AxumPath(record.id), State(state), AuthRequired(user)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_call_record_allowed_for_superuser() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;
        let record = call_record::ActiveModel {
            call_id: Set("del-super-1".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let user = superuser();
        let resp = delete_call_record(AxumPath(record.id), State(state), AuthRequired(user)).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // ── extract_channel_from_wav tests ─────────────────────────────────────

    fn create_test_stereo_wav(path: &str) {
        let spec = WavSpec {
            channels: 2,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(path, spec).unwrap();
        // 10 samples: left = 100, right = 200 for each frame
        for _ in 0..10 {
            writer.write_sample(100i16).unwrap(); // left
            writer.write_sample(200i16).unwrap(); // right
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn extract_channel_extracts_left_channel() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo_left.wav");
        let path_str = path.to_string_lossy().to_string();
        create_test_stereo_wav(&path_str);

        let result = extract_channel_from_wav(&path_str, 0).unwrap();
        assert!(!result.is_empty(), "should return WAV data");

        // Read the mono output and verify it contains only left channel values
        let mut mono_reader = WavReader::new(std::io::Cursor::new(&result)).unwrap();
        assert_eq!(mono_reader.spec().channels, 1, "should be mono");
        assert_eq!(mono_reader.spec().sample_rate, 8000);
        let mono_samples: Vec<i16> = mono_reader
            .samples()
            .collect::<::std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(mono_samples.len(), 10, "should have 10 samples");
        assert!(
            mono_samples.iter().all(|&s| s == 100),
            "all samples should be 100 (left)"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extract_channel_extracts_right_channel() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo_right.wav");
        let path_str = path.to_string_lossy().to_string();
        create_test_stereo_wav(&path_str);

        let result = extract_channel_from_wav(&path_str, 1).unwrap();
        assert!(!result.is_empty(), "should return WAV data");

        let mut mono_reader = WavReader::new(std::io::Cursor::new(&result)).unwrap();
        assert_eq!(mono_reader.spec().channels, 1, "should be mono");
        let mono_samples: Vec<i16> = mono_reader
            .samples()
            .collect::<::std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(mono_samples.len(), 10, "should have 10 samples");
        assert!(
            mono_samples.iter().all(|&s| s == 200),
            "all samples should be 200 (right)"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extract_channel_rejects_mono_wav() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_mono.wav");
        let path_str = path.to_string_lossy().to_string();
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        writer.write_sample(42i16).unwrap();
        writer.finalize().unwrap();

        let result = extract_channel_from_wav(&path_str, 0);
        assert!(result.is_err(), "mono file should be rejected");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extract_channel_rejects_nonexistent_file() {
        let result = extract_channel_from_wav("/nonexistent/file.wav", 0);
        assert!(result.is_err());
    }

    #[test]
    fn extract_channel_output_is_valid_wav() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_stereo_valid.wav");
        let path_str = path.to_string_lossy().to_string();
        create_test_stereo_wav(&path_str);

        let result = extract_channel_from_wav(&path_str, 0).unwrap();

        // Verify WAV header
        assert_eq!(&result[0..4], b"RIFF", "should have RIFF header");
        assert_eq!(&result[8..12], b"WAVE", "should have WAVE format");
        assert_eq!(&result[12..16], b"fmt ", "should have fmt chunk");

        // Verify mono: 1 channel at offset 22 (2 bytes LE)
        let channels = u16::from_le_bytes([result[22], result[23]]);
        assert_eq!(channels, 1, "output should be mono");

        // Verify sample rate
        let rate = u32::from_le_bytes([result[24], result[25], result[26], result[27]]);
        assert_eq!(rate, 8000);

        std::fs::remove_file(&path).ok();
    }

    // ── build_recording_payload tests ──────────────────────────────────────

    #[tokio::test]
    async fn recording_payload_supports_streams_for_local_file_path() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("local-path-rec".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            recording_url: Set(Some("/var/recordings/test.wav".into())),
            recording_duration_secs: Set(Some(10)),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let payload = build_recording_payload(&state, &record, None).await;
        let payload = payload.expect("should return a recording payload");
        assert_eq!(
            payload["supports_streams"], true,
            "local path should support streams"
        );
        assert!(
            payload["url"].as_str().unwrap().contains("/call-records/"),
            "should route through /recording endpoint"
        );
    }

    #[tokio::test]
    async fn recording_payload_no_streams_for_external_url() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("external-url-rec".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            recording_url: Set(Some("https://cdn.example.com/recording.wav".into())),
            recording_duration_secs: Set(Some(10)),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let payload = build_recording_payload(&state, &record, None).await;
        let payload = payload.expect("should return a recording payload");
        assert_eq!(
            payload["supports_streams"], false,
            "external URL should not support streams"
        );
        assert_eq!(
            payload["url"], "https://cdn.example.com/recording.wav",
            "should use URL directly"
        );
    }

    #[tokio::test]
    async fn recording_payload_supports_streams_with_inline_url() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("inline-url-rec".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            recording_url: Set(None),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let inline_url = state.url_for(&format!("/call-records/{}/recording", record.id));
        let payload = build_recording_payload(&state, &record, Some(inline_url.as_str())).await;
        let payload = payload.expect("should return a recording payload");
        assert_eq!(
            payload["supports_streams"], true,
            "inline recording URL should support streams"
        );
    }

    #[tokio::test]
    async fn recording_payload_returns_none_when_no_recording() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("no-recording".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(10),
            recording_url: Set(None),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let payload = build_recording_payload(&state, &record, None).await;
        assert!(
            payload.is_none(),
            "should return None when no recording and no sipflow"
        );
    }

    /// Local artifacts are archived under `{root}/YYYYMMDD[/HH]` after the
    /// call; rows written before that rename store the pre-archive path and
    /// must resolve through the dated fallback.
    #[test]
    fn resolve_archived_artifact_path_falls_back_to_dated_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("20260821")).expect("mkdir daily");
        std::fs::write(root.join("20260821").join("sess.jsonl"), b"{}\n").unwrap();

        let at = Utc.with_ymd_and_hms(2026, 8, 21, 9, 30, 0).unwrap();
        let stale = root.join("sess.jsonl").to_string_lossy().into_owned();
        assert_eq!(
            resolve_archived_artifact_path(&stale, at),
            root.join("20260821")
                .join("sess.jsonl")
                .to_string_lossy()
                .into_owned()
        );

        // A path that already exists resolves to itself.
        let existing = root.join("20260821").join("sess.jsonl");
        assert_eq!(
            resolve_archived_artifact_path(existing.to_str().unwrap(), at),
            existing.to_string_lossy().into_owned()
        );

        // Missing everywhere: return the original untouched.
        let missing = root.join("nope.jsonl").to_string_lossy().into_owned();
        assert_eq!(resolve_archived_artifact_path(&missing, at), missing);
    }

    #[test]
    fn resolve_archived_artifact_path_falls_back_to_hourly_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("20260821").join("15")).expect("mkdir hourly");
        std::fs::write(root.join("20260821").join("15").join("sess.wav"), b"wav").unwrap();

        let at = Utc.with_ymd_and_hms(2026, 8, 21, 15, 5, 0).unwrap();
        let stale = root.join("sess.wav").to_string_lossy().into_owned();
        assert_eq!(
            resolve_archived_artifact_path(&stale, at),
            root.join("20260821")
                .join("15")
                .join("sess.wav")
                .to_string_lossy()
                .into_owned()
        );
    }

    /// A CDR row whose `recording_url` still points at the pre-archive path
    /// must resolve to the archived WAV for playback/download.
    #[tokio::test]
    async fn select_recording_path_resolves_archived_wav_from_stale_url() {
        let db = setup_db().await;

        let started = Utc.with_ymd_and_hms(2026, 8, 21, 9, 25, 21).unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("20260821")).expect("mkdir daily");
        let archived = dir.path().join("20260821").join("sess.wav");
        std::fs::write(&archived, b"wav").unwrap();

        let record = call_record::ActiveModel {
            call_id: Set("stale-rec-path".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(started),
            duration_secs: Set(10),
            recording_url: Set(Some(
                dir.path().join("sess.wav").to_string_lossy().into_owned(),
            )),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(started),
            updated_at: Set(started),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let resolved = select_recording_path(&record, None);
        assert_eq!(
            resolved.as_deref(),
            Some(archived.to_string_lossy().as_ref()),
            "stale pre-archive recording_url must resolve into the daily subdir"
        );
    }

    /// `?segment=` playback: the selector must pin playback to the matching
    /// recorder entry (`unique_id` first, `track_id` fallback) instead of the
    /// first existing file; unknown selectors resolve to `None` so the
    /// handler returns 404 rather than silently playing another segment.
    #[tokio::test]
    async fn select_recording_segment_path_matches_unique_id_then_track_id() {
        let db = setup_db().await;
        let started = Utc.with_ymd_and_hms(2026, 8, 21, 9, 25, 21).unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("call_01_ivr.wav");
        let second = dir.path().join("call_02_agent.wav");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();

        let record = call_record::ActiveModel {
            call_id: Set("segment-playback".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(started),
            duration_secs: Set(30),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(started),
            updated_at: Set(started),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let media = |track_id: &str, unique_id: &str, path: &std::path::Path| {
            crate::callrecord::CallRecordMedia {
                track_id: track_id.into(),
                path: path.to_string_lossy().into_owned(),
                size: 3,
                unique_id: Some(unique_id.into()),
                extra: None,
            }
        };
        let cdr = CdrData {
            record: CallRecord {
                call_id: "segment-playback".into(),
                recorder: vec![
                    media(
                        "segment:ivr:1",
                        "11111111-1111-1111-1111-111111111111",
                        &first,
                    ),
                    media(
                        "segment:agent:ab12",
                        "22222222-2222-2222-2222-222222222222",
                        &second,
                    ),
                ],
                ..Default::default()
            },
            raw_content: String::new(),
            cdr_path: String::new(),
            storage: None,
        };

        // unique_id selector (the id `recording_metadata_available` carries)
        let resolved = select_recording_segment_path(
            &record,
            Some(&cdr),
            Some("22222222-2222-2222-2222-222222222222"),
        );
        assert_eq!(
            resolved.as_deref(),
            Some(second.to_string_lossy().as_ref()),
            "unique_id selector must pick the matching segment"
        );

        // track_id fallback
        let resolved = select_recording_segment_path(&record, Some(&cdr), Some("segment:ivr:1"));
        assert_eq!(
            resolved.as_deref(),
            Some(first.to_string_lossy().as_ref()),
            "track_id selector must pick the matching segment"
        );

        // Unknown / empty / absent selectors
        assert_eq!(
            select_recording_segment_path(&record, Some(&cdr), Some("missing")),
            None,
            "unknown selector must not fall back to another segment"
        );
        assert_eq!(
            select_recording_segment_path(&record, Some(&cdr), Some("  ")),
            None
        );
        assert_eq!(
            select_recording_segment_path(&record, Some(&cdr), None),
            None
        );
    }

    /// Segmented calls expose a per-segment playback list on the detail
    /// payload (local files → `/recording?segment=` URLs); calls with fewer
    /// than two segments keep the legacy payload shape (`None`).
    #[tokio::test]
    async fn recording_segments_payload_lists_segment_playback_urls() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;
        let started = Utc.with_ymd_and_hms(2026, 8, 21, 9, 25, 21).unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("call_01_ivr.wav");
        let second = dir.path().join("call_02_agent.wav");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();

        let record = call_record::ActiveModel {
            call_id: Set("segments-payload".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(started),
            duration_secs: Set(30),
            has_transcript: Set(false),
            transcript_status: Set("pending".into()),
            created_at: Set(started),
            updated_at: Set(started),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert call record");

        let media = |track_id: &str, unique_id: &str, path: &std::path::Path| {
            crate::callrecord::CallRecordMedia {
                track_id: track_id.into(),
                path: path.to_string_lossy().into_owned(),
                size: 3,
                unique_id: Some(unique_id.into()),
                extra: Some(HashMap::from([
                    (
                        "segment_type".to_string(),
                        json!(track_id.split(':').nth(1)),
                    ),
                    ("seq".to_string(), json!(1)),
                ])),
            }
        };
        let cdr = CdrData {
            record: CallRecord {
                call_id: "segments-payload".into(),
                recorder: vec![
                    media(
                        "segment:ivr:1",
                        "11111111-1111-1111-1111-111111111111",
                        &first,
                    ),
                    media(
                        "segment:agent:ab12",
                        "22222222-2222-2222-2222-222222222222",
                        &second,
                    ),
                ],
                ..Default::default()
            },
            raw_content: String::new(),
            cdr_path: String::new(),
            storage: None,
        };

        let payload = build_recording_segments_payload(&state, &record, Some(&cdr))
            .await
            .expect("segments payload");
        let segments = payload.as_array().expect("segments array");
        assert_eq!(segments.len(), 2);
        assert_eq!(
            segments[0]["unique_id"].as_str(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            segments[1]["unique_id"].as_str(),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert_eq!(segments[1]["supports_streams"], json!(true));
        let url = segments[1]["playback_url"].as_str().expect("playback url");
        assert!(
            url.contains(&format!("/call-records/{}/recording", record.id)),
            "segment playback_url must target the console endpoint: {url}"
        );
        assert!(
            url.contains("segment=22222222-2222-2222-2222-222222222222"),
            "segment playback_url must carry the segment selector: {url}"
        );

        // Single-segment calls keep the legacy payload (no `segments` key).
        let single = CdrData {
            record: CallRecord {
                call_id: "segments-payload".into(),
                recorder: vec![media(
                    "segment:ivr:1",
                    "11111111-1111-1111-1111-111111111111",
                    &first,
                )],
                ..Default::default()
            },
            raw_content: String::new(),
            cdr_path: String::new(),
            storage: None,
        };
        assert_eq!(
            build_recording_segments_payload(&state, &record, Some(&single)).await,
            None,
            "calls with fewer than two segments must not grow a segments list"
        );
    }

    // ── session_id primary/child predicates ─────────────────────────────────────

    fn render_condition_sql(filters: Option<QueryCallRecordFilters>) -> String {
        use sea_orm::sea_query::SqliteQueryBuilder;
        let condition = build_condition(&filters);
        let mut stmt = sea_orm::sea_query::SelectStatement::new();
        stmt.from(CallRecordEntity).cond_where(condition);
        stmt.to_string(SqliteQueryBuilder)
    }

    #[test]
    fn build_condition_defaults_to_primary_legs_only() {
        let sql = render_condition_sql(Some(QueryCallRecordFilters::default()));
        assert!(
            sql.contains(r#""session_id" IS NULL"#),
            "default condition must exclude child legs: {sql}"
        );
        assert!(
            sql.contains(r#""session_id" = "rustpbx_call_records"."call_id""#),
            "default condition must keep legacy root rows via session_id = call_id: {sql}"
        );
    }

    #[test]
    fn build_condition_all_legs_includes_child_legs() {
        let sql = render_condition_sql(Some(QueryCallRecordFilters {
            all_legs: Some(true),
            ..Default::default()
        }));
        assert!(
            !sql.contains("session_id"),
            "all_legs must not filter by session_id: {sql}"
        );
    }

    /// Logical-call list view: default filters collapse a 3-leg call (root +
    /// agent leg + transfer leg) to a single primary row; `allLegs` reveals
    /// every leg.
    #[tokio::test]
    async fn query_call_records_defaults_to_one_row_per_logical_call() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let now = Utc::now();
        for (call_id, session_id) in [
            ("root-s1", Some("root-s1".to_string())),
            ("agent-leg-s2", Some("root-s1".to_string())),
            ("transfer-leg-s3", Some("root-s1".to_string())),
        ] {
            call_record::ActiveModel {
                call_id: Set(call_id.into()),
                session_id: Set(session_id),
                direction: Set("inbound".into()),
                status: Set("completed".into()),
                started_at: Set(now),
                duration_secs: Set(30),
                has_transcript: Set(false),
                transcript_status: Set("none".into()),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(&db)
            .await
            .expect("insert leg");
        }

        // Default: one row per logical call — the primary only.
        let response = query_call_records(
            State(state.clone()),
            AuthRequired(superuser()),
            Json(forms::ListQuery::<QueryCallRecordFilters> {
                filters: Some(QueryCallRecordFilters::default()),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["total_items"].as_i64(),
            Some(1),
            "default list must show only the primary CDR: {payload}"
        );
        assert_eq!(payload["items"][0]["call_id"], "root-s1");
        assert_eq!(payload["items"][0]["leg_role"], "primary");
        assert_eq!(payload["items"][0]["session_id"], "root-s1");

        // allLegs: every leg of the logical call.
        let response = query_call_records(
            State(state),
            AuthRequired(superuser()),
            Json(forms::ListQuery::<QueryCallRecordFilters> {
                filters: Some(QueryCallRecordFilters {
                    all_legs: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["total_items"].as_i64(),
            Some(3),
            "all_legs must reveal every CDR leg: {payload}"
        );
    }

    /// by-session artifacts must return the root row plus every child leg of the
    /// logical call (legacy behavior only ever matched the root row itself).
    #[tokio::test]
    async fn list_session_artifacts_returns_all_legs() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let now = Utc::now();
        for (call_id, session_id) in [
            ("root-art", Some("root-art".to_string())),
            ("agent-art", Some("root-art".to_string())),
            ("transfer-art", Some("root-art".to_string())),
        ] {
            call_record::ActiveModel {
                call_id: Set(call_id.into()),
                session_id: Set(session_id),
                direction: Set("inbound".into()),
                status: Set("completed".into()),
                started_at: Set(now),
                duration_secs: Set(5),
                has_transcript: Set(false),
                transcript_status: Set("none".into()),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(&db)
            .await
            .expect("insert leg");
        }

        let response = list_session_artifacts(
            AxumPath("root-art".to_string()),
            State(state),
            AuthRequired(superuser()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        let legs = payload["legs"].as_array().expect("legs array");
        assert_eq!(legs.len(), 3, "artifacts must list every leg: {payload}");
        let roles: Vec<&str> = legs
            .iter()
            .filter_map(|leg| leg["leg_role"].as_str())
            .collect();
        assert_eq!(
            roles,
            vec!["primary", "child", "child"],
            "legs must carry primary/child roles ordered by start time"
        );
    }
}
