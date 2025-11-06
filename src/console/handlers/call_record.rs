use crate::callrecord::{
    CallRecord,
    sipflow::{SipFlowDirection, SipMessageItem},
    storage::{self, CdrStorage},
};
use crate::config::TranscriptConfig;
use crate::console::{ConsoleState, handlers::forms, middleware::AuthRequired};
use anyhow::{Context as AnyhowContext, Result as AnyResult};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{self, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, NaiveDate, SecondsFormat, TimeZone, Utc};
use sea_orm::sea_query::{Expr, Order};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection, DbErr,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value, json};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::time::timeout;
use tokio_util::io::ReaderStream;
use tracing::{info, warn};
use urlencoding::encode;
use uuid::Uuid;

use super::utils::{build_sensevoice_transcribe_command, command_exists, model_file_path};

const BILLING_STATUS_CHARGED: &str = "charged";
const BILLING_STATUS_INCLUDED: &str = "included";
const BILLING_STATUS_ZERO_DURATION: &str = "zero-duration";
const BILLING_STATUS_UNRATED: &str = "unrated";

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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct QueryCallRecordFilters {
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
    tags: Option<Vec<String>>,
    #[serde(default)]
    billing_status: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct DownloadRequest {
    path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TranscriptRequest {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCallRecordPayload {
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    note: Option<UpdateCallRecordNote>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCallRecordNote {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTranscript {
    version: u8,
    source: String,
    generated_at: DateTime<Utc>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration_secs: Option<f64>,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    segments: Vec<StoredTranscriptSegment>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    analysis: Option<StoredTranscriptAnalysis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTranscriptSegment {
    #[serde(default)]
    idx: Option<u32>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    end: Option<f64>,
    #[serde(default)]
    channel: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTranscriptAnalysis {
    #[serde(default)]
    elapsed: Option<f64>,
    #[serde(default)]
    rtf: Option<f64>,
    #[serde(default)]
    word_count: usize,
    #[serde(default)]
    asr_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SenseVoiceCliSegment {
    #[serde(default)]
    start_sec: Option<f64>,
    #[serde(default)]
    end_sec: Option<f64>,
    #[serde(default)]
    text: String,
    #[serde(default, rename = "tags")]
    _tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SenseVoiceCliChannel {
    #[serde(default)]
    channel: Option<u32>,
    #[serde(default)]
    duration_sec: Option<f64>,
    #[serde(default)]
    rtf: Option<f64>,
    #[serde(default)]
    segments: Vec<SenseVoiceCliSegment>,
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
            "/call-records/{id}/transcript",
            get(get_call_record_transcript).post(trigger_call_record_transcript),
        )
        .route(
            "/call-records/{id}/metadata",
            get(download_call_record_metadata),
        )
        .route(
            "/call-records/{id}/sip-flow",
            get(download_call_record_sip_flow),
        )
        .route("/call-records/{id}/recording", get(stream_call_recording))
}
async fn stream_call_recording(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    headers: HeaderMap,
) -> Response {
    let db = state.db();
    let record = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(id = pk, "failed to load call record for playback: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load call record" })),
            )
                .into_response();
        }
    };

    let cdr_data = load_cdr_data(&state, &record).await;
    let recording_path = match select_recording_path(&record, cdr_data.as_ref()) {
        Some(path) => path,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Recording file not found" })),
            )
                .into_response();
        }
    };

    let file_meta = match tokio::fs::metadata(&recording_path).await {
        Ok(meta) if meta.is_file() => meta,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Recording file not found" })),
            )
                .into_response();
        }
    };

    let file_len = file_meta.len();
    if file_len == 0 {
        return (
            StatusCode::NO_CONTENT,
            Json(json!({ "message": "Recording file is empty" })),
        )
            .into_response();
    }

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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to open recording file" })),
            )
                .into_response();
        }
    };

    if start > 0 {
        if let Err(err) = file.seek(std::io::SeekFrom::Start(start)).await {
            warn!(path = %recording_path, "failed to seek recording file: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to read recording file" })),
            )
                .into_response();
        }
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
    let safe_file_name = file_name.replace('"', "'");
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

fn safe_download_filename(path: &str, fallback: &str) -> String {
    let candidate = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback);

    let sanitized: String = candidate
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\n' | '\r' | '\t' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    if sanitized.trim().is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

async fn download_call_record_metadata(
    AxumPath(pk): AxumPath<i64>,
    Query(params): Query<DownloadRequest>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(id = pk, "failed to load call record metadata: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load call record" })),
            )
                .into_response();
        }
    };

    let cdr_data = match load_cdr_data(&state, &model).await {
        Some(data) => data,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Metadata file not found" })),
            )
                .into_response();
        }
    };

    if let Some(requested) = params
        .path
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if requested != cdr_data.cdr_path {
            warn!(
                id = pk,
                requested_path = requested,
                actual_path = %cdr_data.cdr_path,
                "metadata download path mismatch"
            );
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Metadata file not found" })),
            )
                .into_response();
        }
    }

    let filename = safe_download_filename(&cdr_data.cdr_path, &format!("call-record-{}.json", pk));

    let raw = cdr_data.raw_content;
    let len_header = raw.len().to_string();

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    if let Ok(value) = HeaderValue::from_str(&len_header) {
        headers.insert(http::header::CONTENT_LENGTH, value);
    }
    if let Ok(disposition) =
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"", filename))
    {
        headers.insert(http::header::CONTENT_DISPOSITION, disposition);
    }

    (headers, Body::from(raw)).into_response()
}

async fn download_call_record_sip_flow(
    AxumPath(pk): AxumPath<i64>,
    Query(params): Query<DownloadRequest>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(id = pk, "failed to load call record sip flow: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load call record" })),
            )
                .into_response();
        }
    };

    let cdr_data = match load_cdr_data(&state, &model).await {
        Some(data) => data,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "SIP flow file not found" })),
            )
                .into_response();
        }
    };

    let requested_path = params
        .path
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());

    let selected_path = if let Some(path) = requested_path {
        if cdr_data
            .sip_flow_paths
            .iter()
            .any(|entry| entry.path == path)
        {
            path.to_string()
        } else {
            warn!(
                id = pk,
                requested_path = path,
                "sip flow path not registered for record"
            );
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "SIP flow file not found" })),
            )
                .into_response();
        }
    } else if let Some(entry) = cdr_data.sip_flow_paths.first() {
        entry.path.clone()
    } else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "SIP flow file not found" })),
        )
            .into_response();
    };

    let mut bytes: Option<Vec<u8>> = None;

    if let Some(storage_ref) = cdr_data.storage.as_ref() {
        match storage_ref.read_bytes(&selected_path).await {
            Ok(data) => bytes = Some(data),
            Err(err) => {
                warn!(path = %selected_path, "failed to download sip flow from storage: {}", err);
            }
        }
    }

    if bytes.is_none() {
        match tokio::fs::read(&selected_path).await {
            Ok(data) => bytes = Some(data),
            Err(err) => {
                warn!(path = %selected_path, "failed to read sip flow file: {}", err);
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "message": "SIP flow file not found" })),
                )
                    .into_response();
            }
        }
    }

    let data = bytes.unwrap_or_default();
    let filename =
        safe_download_filename(&selected_path, &format!("call-record-{}-sip-flow.json", pk));
    let len_header = data.len().to_string();

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    if let Ok(value) = HeaderValue::from_str(&len_header) {
        headers.insert(http::header::CONTENT_LENGTH, value);
    }
    if let Ok(disposition) =
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"", filename))
    {
        headers.insert(http::header::CONTENT_DISPOSITION, disposition);
    }

    (headers, Body::from(data)).into_response()
}

async fn page_call_records(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let filters = match load_filters(state.db()).await {
        Ok(filters) => filters,
        Err(err) => {
            warn!("failed to load call record filters: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load call records" })),
            )
                .into_response();
        }
    };

    state.render(
        "console/call_records.html",
        json!({
            "nav_active": "call-records",
            "base_path": state.base_path(),
            "filter_options": filters,
            "list_url": state.url_for("/call-records"),
            "page_size_options": vec![10, 25, 50],
        }),
    )
}

async fn query_call_records(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::ListQuery<QueryCallRecordFilters>>,
) -> Response {
    let db = state.db();
    let filters = payload.filters.clone();
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

    let paginator = selector.paginate(db, payload.normalize().1);
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

    let related = match load_related_context(db, &pagination.items).await {
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

    let items: Vec<Value> = pagination
        .items
        .iter()
        .zip(inline_recordings.iter())
        .map(|(record, inline)| build_record_payload(record, &related, &state, inline.as_deref()))
        .collect();

    let summary = match build_summary(db, condition).await {
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

async fn get_call_record_transcript(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let record = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(
                id = pk,
                "failed to load call record for transcript: {}", err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let cdr_data = load_cdr_data(&state, &record).await;
    match load_stored_transcript(&record, cdr_data.as_ref()).await {
        Ok(Some(stored)) => {
            let payload = build_transcript_payload_value(&record, Some(&stored));
            Json(json!({
                "status": record.transcript_status,
                "transcript": payload,
            }))
            .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Transcript not available" })),
        )
            .into_response(),
        Err(err) => {
            warn!(call_id = %record.call_id, "failed to load stored transcript: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load transcript" })),
            )
                .into_response()
        }
    }
}

async fn trigger_call_record_transcript(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<TranscriptRequest>,
) -> Response {
    let db = state.db();
    let mut record = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(
                id = pk,
                "failed to load call record for transcription: {}", err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let TranscriptRequest { force, language } = payload;

    let app_state = match state.app_state() {
        Some(app) => app,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "message": "Application state unavailable" })),
            )
                .into_response();
        }
    };

    let transcript_cfg = match app_state
        .config
        .proxy
        .as_ref()
        .and_then(|cfg| cfg.transcript.as_ref())
    {
        Some(cfg) => cfg.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "SenseVoice CLI transcription is not configured" })),
            )
                .into_response();
        }
    };

    let cdr_data = load_cdr_data(&state, &record).await;
    let recording_path = match select_recording_path(&record, cdr_data.as_ref()) {
        Some(path) => path,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Recording file not found for this call" })),
            )
                .into_response();
        }
    };

    if record.has_transcript && !force {
        if let Ok(Some(stored)) = load_stored_transcript(&record, cdr_data.as_ref()).await {
            let payload = build_transcript_payload_value(&record, Some(&stored));
            return Json(json!({
                "status": record.transcript_status,
                "transcript": payload,
            }))
            .into_response();
        }
    }

    if record.transcript_status.eq_ignore_ascii_case("processing") && !force {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "message": "Transcription already in progress" })),
        )
            .into_response();
    }

    let asr_model = Some("sensevoice-cli".to_string());
    let command = transcript_cfg
        .command
        .as_deref()
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty())
        .unwrap_or("sensevoice-cli")
        .to_string();

    if !command_exists(&command) {
        return (
            StatusCode::FAILED_DEPENDENCY,
            Json(json!({
                "message": format!(
                    "sensevoice-cli is not available (looked for '{}'). Install via `cargo install sensevoice-cli` or configure proxy.transcript.command.",
                    command
                )
            })),
        )
            .into_response();
    }

    let models_path = match resolve_models_path(&transcript_cfg) {
        Some(path) => path,
        None => {
            return (
                StatusCode::FAILED_DEPENDENCY,
                Json(json!({
                    "message": "SenseVoice model path is not configured. Set MODEL_PATH or proxy.transcript.models_path and download the model before transcribing.",
                })),
            )
                .into_response();
        }
    };

    let model_file = model_file_path(&models_path);
    if tokio::fs::metadata(&model_file).await.is_err() {
        return (
            StatusCode::FAILED_DEPENDENCY,
            Json(json!({
                "message": format!(
                    "SenseVoice model not found at {}. Download it from Settings → ASR integrations before retrying.",
                    model_file.display()
                )
            })),
        )
            .into_response();
    }

    let storage = cdr_data.as_ref().and_then(|cdr| cdr.storage.clone());

    let transcript_storage_path = if let Some(cdr_ref) = cdr_data.as_ref() {
        let mut path = PathBuf::from(&cdr_ref.cdr_path);
        path.set_extension("transcript.json");
        path.to_string_lossy().into_owned()
    } else {
        let mut path = PathBuf::from(&recording_path);
        path.set_extension("transcript.json");
        path.to_string_lossy().into_owned()
    };

    let local_transcript_path = match storage.as_ref() {
        Some(storage_ref) => storage_ref
            .local_full_path(&transcript_storage_path)
            .unwrap_or_else(|| {
                let mut temp_path = env::temp_dir();
                temp_path.push(format!(
                    "rustpbx-{}-{}.transcript.json",
                    record.call_id,
                    Uuid::new_v4()
                ));
                temp_path
            }),
        None => PathBuf::from(&transcript_storage_path),
    };

    if let Some(parent) = local_transcript_path.parent() {
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            warn!(dir = %parent.display(), "failed to ensure transcript directory: {}", err);
        }
    }

    if tokio::fs::metadata(&local_transcript_path).await.is_ok() {
        if let Err(err) = tokio::fs::remove_file(&local_transcript_path).await {
            warn!(
                file = %local_transcript_path.display(),
                "failed to remove existing transcript before transcription: {}",
                err
            );
        }
    }

    let transcript_output_path = local_transcript_path.to_string_lossy().into_owned();

    let now = Utc::now();
    if let Err(err) = (CallRecordActiveModel {
        id: Set(record.id),
        transcript_status: Set("processing".to_string()),
        updated_at: Set(now),
        ..Default::default()
    })
    .update(db)
    .await
    {
        warn!(call_id = %record.call_id, "failed to update transcript status: {}", err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": "Failed to update call record" })),
        )
            .into_response();
    }
    record.transcript_status = "processing".to_string();
    record.updated_at = now;
    let mut cmd: tokio::process::Command = build_sensevoice_transcribe_command(
        &command,
        &recording_path,
        Some(models_path.as_str()),
        Some(transcript_output_path.as_str()),
    );
    let command_args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let start_instant = Instant::now();
    let output_result = if let Some(timeout_secs) = transcript_cfg.timeout_secs {
        match timeout(Duration::from_secs(timeout_secs), cmd.output()).await {
            Ok(result) => result,
            Err(_) => {
                warn!(call_id = %record.call_id, timeout_secs, "sensevoice-cli timed out");
                let _ = (CallRecordActiveModel {
                    id: Set(record.id),
                    transcript_status: Set("failed".to_string()),
                    updated_at: Set(Utc::now()),
                    ..Default::default()
                })
                .update(db)
                .await;
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({
                        "message": format!(
                            "sensevoice-cli timed out after {} seconds",
                            timeout_secs
                        )
                    })),
                )
                    .into_response();
            }
        }
    } else {
        cmd.output().await
    };

    let output = match output_result {
        Ok(output) => output,
        Err(err) => {
            warn!(call_id = %record.call_id, "sensevoice-cli failed to execute: {}", err);
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "message": format!("Failed to execute sensevoice-cli: {}", err) })),
            )
                .into_response();
        }
    };

    if !output.status.success() {
        let stderr_full = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stderr_preview = stderr_full.chars().take(160).collect::<String>();
        let mut client_message = "sensevoice-cli transcription failed".to_string();
        let mut status_code = StatusCode::BAD_GATEWAY;

        if stderr_full.to_ascii_lowercase().contains("unsupported")
            && stderr_full.to_ascii_lowercase().contains("codec")
        {
            client_message = "SenseVoice CLI cannot process this recording codec. Convert the audio to PCM (16 kHz) or enable a supported codec before retrying.".to_string();
            status_code = StatusCode::UNPROCESSABLE_ENTITY;
        }
        warn!(
            call_id = %record.call_id,
            %command,
            args = %command_args.join(" "),
            code = output.status.code(),
            stderr = %stderr_preview,
            "sensevoice-cli exited with failure"
        );
        let _ = (CallRecordActiveModel {
            id: Set(record.id),
            transcript_status: Set("failed".to_string()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        })
        .update(db)
        .await;
        return (status_code, Json(json!({ "message": client_message }))).into_response();
    }

    let elapsed_secs = start_instant.elapsed().as_secs_f64();

    info!(
        call_id = %record.call_id,
        %command,
        args = %command_args.join(" "),
        elapsed_secs,
        "sensevoice-cli transcription completed successfully"
    );

    let transcript_bytes = match tokio::fs::read(&local_transcript_path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(
                call_id = %record.call_id,
                file = %local_transcript_path.display(),
                "sensevoice-cli succeeded but transcript file missing: {}",
                err
            );
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "message": "SenseVoice CLI did not produce transcript data" })),
            )
                .into_response();
        }
    };

    let cli_output: Vec<SenseVoiceCliChannel> = match serde_json::from_slice(&transcript_bytes) {
        Ok(parsed) => parsed,
        Err(err) => {
            let preview = String::from_utf8_lossy(&transcript_bytes)
                .chars()
                .take(160)
                .collect::<String>();
            warn!(
                call_id = %record.call_id,
                error = %err,
                file = %local_transcript_path.display(),
                preview = %preview,
                "failed to parse sensevoice-cli transcript file"
            );
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "message": "Invalid sensevoice-cli output" })),
            )
                .into_response();
        }
    };

    let mut stored_segments: Vec<StoredTranscriptSegment> = Vec::new();
    let mut idx_counter: u32 = 0;
    let mut max_end = 0.0_f64;
    let mut rtfs: Vec<f64> = Vec::new();
    let mut durations: Vec<f64> = Vec::new();

    for channel in cli_output.into_iter() {
        if let Some(rtf_value) = channel.rtf {
            rtfs.push(rtf_value);
        }
        if let Some(duration_value) = channel.duration_sec {
            durations.push(duration_value);
        }
        for segment in channel.segments.into_iter() {
            let text = segment.text.trim();
            if text.is_empty() {
                continue;
            }
            if let Some(end) = segment.end_sec {
                if end > max_end {
                    max_end = end;
                }
            }
            stored_segments.push(StoredTranscriptSegment {
                idx: Some(idx_counter),
                text: text.to_string(),
                start: segment.start_sec,
                end: segment.end_sec,
                channel: channel.channel,
            });
            idx_counter += 1;
        }
    }

    if stored_segments.is_empty() {
        info!(
            call_id = %record.call_id,
            "sensevoice-cli returned no transcript segments; treating as empty transcript"
        );
    }

    let average_rtf = if rtfs.is_empty() {
        None
    } else {
        Some(rtfs.iter().copied().sum::<f64>() / rtfs.len() as f64)
    };

    let audio_duration = durations.into_iter().fold(None, |acc: Option<f64>, value| {
        Some(acc.map_or(value, |current| current.max(value)))
    });

    let elapsed = Some(elapsed_secs);
    let rtf = average_rtf;

    let mut dedup = HashSet::new();
    let mut text_parts: Vec<String> = Vec::new();
    for segment in &stored_segments {
        let trimmed = segment.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = format!(
            "{}::{:.3}-{:.3}",
            trimmed,
            segment.start.unwrap_or_default(),
            segment.end.unwrap_or_default()
        );
        if dedup.insert(key) {
            text_parts.push(trimmed.to_string());
        }
    }

    let transcript_text = text_parts.join(" ");
    let word_count = transcript_text.split_whitespace().count();
    let max_end = stored_segments
        .iter()
        .filter_map(|segment| segment.end)
        .fold(0.0_f64, f64::max);
    let duration_secs = audio_duration.or_else(|| if max_end > 0.0 { Some(max_end) } else { None });

    let analysis = StoredTranscriptAnalysis {
        elapsed,
        rtf,
        word_count,
        asr_model,
    };

    let language_final = language
        .clone()
        .or(record.transcript_language.clone())
        .or(transcript_cfg.default_language.clone());
    let stored_transcript = StoredTranscript {
        version: 1,
        source: "sensevoice-cli".to_string(),
        generated_at: Utc::now(),
        language: language_final.clone(),
        duration_secs,
        sample_rate: transcript_cfg.samplerate,
        segments: stored_segments,
        text: transcript_text.clone(),
        analysis: Some(analysis),
    };

    let excerpt = transcript_excerpt(&transcript_text, 240);

    let serialized_transcript = match serde_json::to_vec_pretty(&stored_transcript) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(call_id = %record.call_id, "failed to serialize transcript: {}", err);
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to serialize transcript" })),
            )
                .into_response();
        }
    };

    let transcript_metadata_path = if let Some(storage_ref) = storage.as_ref() {
        match storage_ref
            .write_bytes(&transcript_storage_path, &serialized_transcript)
            .await
        {
            Ok(stored_path) => {
                if !storage_ref.is_local() {
                    if let Err(err) = tokio::fs::remove_file(&local_transcript_path).await {
                        if err.kind() != ErrorKind::NotFound {
                            warn!(
                                file = %local_transcript_path.display(),
                                "failed to remove transient transcript file: {}",
                                err
                            );
                        }
                    }
                }
                stored_path
            }
            Err(err) => {
                warn!(
                    call_id = %record.call_id,
                    path = %transcript_storage_path,
                    "failed to persist transcript via call record storage: {}",
                    err
                );
                let _ = (CallRecordActiveModel {
                    id: Set(record.id),
                    transcript_status: Set("failed".to_string()),
                    updated_at: Set(Utc::now()),
                    ..Default::default()
                })
                .update(db)
                .await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "message": "Failed to persist transcript" })),
                )
                    .into_response();
            }
        }
    } else {
        if let Err(err) = tokio::fs::write(&local_transcript_path, &serialized_transcript).await {
            warn!(
                file = %local_transcript_path.display(),
                "failed to write transcript file: {}",
                err
            );
            let _ = (CallRecordActiveModel {
                id: Set(record.id),
                transcript_status: Set("failed".to_string()),
                updated_at: Set(Utc::now()),
                ..Default::default()
            })
            .update(db)
            .await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to persist transcript" })),
            )
                .into_response();
        }
        local_transcript_path.to_string_lossy().into_owned()
    };

    update_cdr_with_transcript(
        cdr_data.as_ref(),
        &stored_transcript,
        &transcript_metadata_path,
        &excerpt,
    )
    .await;

    let mut metadata_map = match record.metadata.clone() {
        Some(Value::Object(map)) => map,
        _ => JsonMap::new(),
    };
    metadata_map.insert(
        "transcript".to_string(),
        json!({
            "file": transcript_metadata_path,
            "language": language_final,
            "generated_at": stored_transcript.generated_at.to_rfc3339(),
            "duration_secs": stored_transcript.duration_secs,
            "excerpt": excerpt,
            "model": stored_transcript
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.asr_model.clone()),
            "word_count": stored_transcript
                .analysis
                .as_ref()
                .map(|analysis| analysis.word_count),
        }),
    );
    let metadata_value = if metadata_map.is_empty() {
        None
    } else {
        Some(Value::Object(metadata_map))
    };

    let mut analytics_map = match record.analytics.clone() {
        Some(Value::Object(map)) => map,
        _ => JsonMap::new(),
    };
    analytics_map.insert(
        "transcript".to_string(),
        json!({
            "segment_count": stored_transcript.segments.len(),
            "word_count": stored_transcript
                .analysis
                .as_ref()
                .map(|analysis| analysis.word_count)
                .unwrap_or(word_count),
            "duration_secs": stored_transcript.duration_secs,
            "model": stored_transcript
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.asr_model.clone()),
            "elapsed_secs": stored_transcript
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.elapsed),
            "rtf": stored_transcript
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.rtf),
        }),
    );
    let analytics_value = if analytics_map.is_empty() {
        None
    } else {
        Some(Value::Object(analytics_map))
    };

    let updated_at = Utc::now();
    if let Err(err) = (CallRecordActiveModel {
        id: Set(record.id),
        has_transcript: Set(true),
        transcript_status: Set("completed".to_string()),
        transcript_language: Set(stored_transcript.language.clone()),
        metadata: Set(metadata_value.clone()),
        analytics: Set(analytics_value.clone()),
        updated_at: Set(updated_at),
        ..Default::default()
    })
    .update(db)
    .await
    {
        warn!(call_id = %record.call_id, "failed to update call record after transcription: {}", err);
    }

    record.has_transcript = true;
    record.transcript_status = "completed".to_string();
    record.transcript_language = stored_transcript.language.clone();
    record.metadata = metadata_value;
    record.analytics = analytics_value;
    record.updated_at = updated_at;

    info!(call_id = %record.call_id, "transcript generated successfully");

    let payload = build_transcript_payload_value(&record, Some(&stored_transcript));
    Json(json!({
        "status": record.transcript_status,
        "transcript": payload,
    }))
    .into_response()
}

async fn page_call_record_detail(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match CallRecordEntity::find()
        .filter(CallRecordColumn::Id.eq(pk))
        .one(db)
        .await
    {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load call record '{}': {}", pk, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let related = match load_related_context(db, &[model.clone()]).await {
        Ok(related) => related,
        Err(err) => {
            warn!(
                "failed to load related data for call record '{}': {}",
                pk, err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": err.to_string() })),
            )
                .into_response();
        }
    };

    let cdr_data = load_cdr_data(&state, &model).await;
    let stored_transcript = match load_stored_transcript(&model, cdr_data.as_ref()).await {
        Ok(value) => value,
        Err(err) => {
            warn!(
                id = pk,
                call_id = %model.call_id,
                "failed to load stored transcript for detail view: {}",
                err
            );
            None
        }
    };

    let mut sip_flow_items: Vec<LeggedSipMessage> = if let Some(ref cdr) = cdr_data {
        load_sip_flow_from_files(cdr.storage.as_ref(), &cdr.sip_flow_paths).await
    } else {
        Vec::new()
    };

    if sip_flow_items.is_empty() {
        sip_flow_items = load_sip_flow_from_metadata(&model).await;
    }

    if sip_flow_items.is_empty() {
        if let Some(server) = state.sip_server() {
            let mut call_ids: Vec<String> = vec![model.call_id.clone()];
            if let Some(signaling) = model.signaling.as_ref() {
                if let Some(legs) = signaling.get("legs").and_then(|value| value.as_array()) {
                    for leg in legs {
                        if let Some(call_id) = leg.get("call_id").and_then(|value| value.as_str()) {
                            let trimmed = call_id.trim();
                            if !trimmed.is_empty() {
                                call_ids.push(trimmed.to_string());
                            }
                        }
                    }
                }
            }

            let mut visited = HashSet::new();
            let mut snapshot_items = Vec::new();

            for call_id in call_ids {
                if !visited.insert(call_id.clone()) {
                    continue;
                }
                if let Some(snapshot) = server.sip_flow_snapshot(&call_id) {
                    for item in snapshot {
                        snapshot_items.push(LeggedSipMessage {
                            leg: Some(call_id.clone()),
                            leg_role: None,
                            item,
                        });
                    }
                }
            }

            if !snapshot_items.is_empty() {
                snapshot_items.sort_by(|a, b| a.item.timestamp.cmp(&b.item.timestamp));
                sip_flow_items = snapshot_items;
            }
        }
    }

    if sip_flow_items.len() > 1 {
        sip_flow_items.sort_by(|a, b| a.item.timestamp.cmp(&b.item.timestamp));
    }

    let payload = build_detail_payload(
        &model,
        &related,
        &state,
        sip_flow_items,
        cdr_data.as_ref(),
        stored_transcript.as_ref(),
    );

    state.render(
        "console/call_record_detail.html",
        json!({
            "nav_active": "call-records",
            "page_title": format!("Call record · {}", pk),
            "call_id": model.call_id,
            "call_data": serde_json::to_string(&payload).unwrap_or_default(),
        }),
    )
}

async fn update_call_record(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
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
    let mut record = match CallRecordEntity::find_by_id(pk).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Call record not found" })),
            )
                .into_response();
        }
        Err(err) => {
            warn!(
                call_record_id = pk,
                "failed to load call record for update: {}", err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load call record" })),
            )
                .into_response();
        }
    };

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

    let mut metadata_value = record.metadata.clone();
    if let Some(note_payload) = payload.note.as_ref() {
        let mut metadata_map = match metadata_value.as_ref() {
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                warn!(
                    call_record_id = pk,
                    "unexpected metadata format; resetting for note update"
                );
                JsonMap::new()
            }
            None => JsonMap::new(),
        };

        let text = note_payload
            .text
            .as_ref()
            .map(|value| value.trim())
            .unwrap_or("")
            .to_string();

        if text.is_empty() {
            if metadata_map.remove("console_note").is_some() {
                metadata_value = if metadata_map.is_empty() {
                    None
                } else {
                    Some(Value::Object(metadata_map.clone()))
                };
                active.metadata = Set(metadata_value.clone());
                changed = true;
            }
        } else {
            let updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let updated_by = {
                let username = user.username.trim();
                if username.is_empty() {
                    user.email.clone()
                } else {
                    user.username.clone()
                }
            };
            let note_value = json!({
                "text": text,
                "updated_at": updated_at,
                "updated_by": updated_by,
            });
            let existing_equal = metadata_map
                .get("console_note")
                .map(|value| value == &note_value)
                .unwrap_or(false);
            if !existing_equal {
                metadata_map.insert("console_note".to_string(), note_value);
                metadata_value = Some(Value::Object(metadata_map.clone()));
                active.metadata = Set(metadata_value.clone());
                changed = true;
            }
        }
    }

    if !changed {
        let response = json!({
            "status": "noop",
            "record": {
                "id": record.id,
                "tags": extract_tags(&record.tags),
            },
            "notes": build_console_note_payload(&metadata_value),
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
                Json(json!({ "message": "Failed to update call record" })),
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
        "notes": build_console_note_payload(&updated_record.metadata),
    });
    Json(response).into_response()
}

async fn delete_call_record(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
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

    let tags = load_distinct_tags(db).await?;

    Ok(json!({
        "status": ["any", "completed", "missed", "failed"],
        "direction": ["any", "inbound", "outbound", "internal"],
        "departments": departments,
        "sip_trunks": sip_trunks,
        "tags": tags,
        "billing_status": [
            "any",
            BILLING_STATUS_CHARGED,
            BILLING_STATUS_INCLUDED,
            BILLING_STATUS_ZERO_DURATION,
            BILLING_STATUS_UNRATED,
        ],
    }))
}

async fn load_distinct_tags(db: &DatabaseConnection) -> Result<Vec<String>, DbErr> {
    let rows: Vec<Option<Value>> = CallRecordEntity::find()
        .select_only()
        .column(CallRecordColumn::Tags)
        .filter(CallRecordColumn::Tags.is_not_null())
        .into_tuple::<Option<Value>>()
        .all(db)
        .await?;

    let mut tags: HashSet<String> = HashSet::new();
    for value in rows.into_iter().flatten() {
        if let Some(array) = value.as_array() {
            for tag in array {
                if let Some(tag_str) = tag.as_str() {
                    let trimmed = tag_str.trim();
                    if !trimmed.is_empty() {
                        tags.insert(trimmed.to_string());
                    }
                }
            }
        }
    }

    let mut list: Vec<String> = tags.into_iter().collect();
    list.sort();
    Ok(list)
}

fn build_condition(filters: &Option<QueryCallRecordFilters>) -> Condition {
    let mut condition = Condition::all();

    if let Some(filters) = filters {
        if let Some(q_raw) = filters.q.as_ref() {
            let trimmed = q_raw.trim();
            if !trimmed.is_empty() {
                let mut any_match = Condition::any();
                any_match = any_match.add(CallRecordColumn::CallId.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::DisplayId.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::FromNumber.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::ToNumber.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::CallerName.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::AgentName.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::Queue.contains(trimmed));
                any_match = any_match.add(CallRecordColumn::SipGateway.contains(trimmed));
                condition = condition.add(any_match);
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

        if let Some(from) = parse_date(filters.date_from.as_ref(), false) {
            condition = condition.add(CallRecordColumn::StartedAt.gte(from));
        }
        if let Some(to) = parse_date(filters.date_to.as_ref(), true) {
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

        if let Some(billing_status_raw) = filters.billing_status.as_ref() {
            let trimmed = billing_status_raw.trim();
            if !trimmed.is_empty() && !equals_ignore_ascii_case(trimmed, "any") {
                condition = condition.add(CallRecordColumn::BillingStatus.eq(trimmed));
            }
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
    match storage::resolve_storage(app.config.callrecord.as_ref()) {
        Ok(storage) => storage,
        Err(err) => {
            warn!("failed to resolve call record storage: {}", err);
            None
        }
    }
}

#[derive(Debug, Clone)]
struct SipFlowFileRef {
    leg: Option<String>,
    leg_role: Option<String>,
    path: String,
}

#[derive(Debug, Clone)]
struct LeggedSipMessage {
    leg: Option<String>,
    leg_role: Option<String>,
    item: SipMessageItem,
}

struct SipFlowContext {
    caller_label: String,
    pbx_label: String,
    callee_label: String,
}

fn resolve_leg_roles(record: &CallRecordModel, cdr: Option<&CdrData>) -> HashMap<String, String> {
    let mut roles = HashMap::new();

    if let Some(cdr) = cdr {
        collect_leg_roles_from_cdr(&cdr.record, "primary", &mut roles);
    }

    if let Some(signaling) = record.signaling.as_ref() {
        collect_leg_roles_from_value(signaling, &mut roles);
    }

    if let Some(metadata) = record.metadata.as_ref() {
        collect_leg_roles_from_metadata(metadata, &mut roles);
    }

    if !roles.contains_key(&record.call_id) {
        roles.insert(record.call_id.clone(), "primary".to_string());
    }

    roles
}

fn collect_leg_roles_from_cdr(
    record: &CallRecord,
    role: &str,
    target: &mut HashMap<String, String>,
) {
    let call_id = record.call_id.trim();
    if !call_id.is_empty() {
        target
            .entry(call_id.to_string())
            .or_insert_with(|| role.to_string());
    }

    if let Some(refer) = record.refer_callrecord.as_ref() {
        collect_leg_roles_from_cdr(refer, "b2bua", target);
    }
}

fn collect_leg_roles_from_value(value: &Value, target: &mut HashMap<String, String>) {
    if let Some(legs) = value.get("legs").and_then(|v| v.as_array()) {
        for leg in legs {
            let call_id = leg
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            if let Some(id) = call_id {
                let role = leg
                    .get("role")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "b2bua".to_string());
                target.entry(id.to_string()).or_insert(role);
            }
        }
    }
}

fn collect_leg_roles_from_metadata(value: &Value, target: &mut HashMap<String, String>) {
    if let Some(entries) = value.get("sip_flow_files").and_then(|item| item.as_array()) {
        for entry in entries {
            let call_id = entry
                .get("leg")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let role = entry
                .get("role")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty());
            if let (Some(id), Some(role)) = (call_id, role) {
                target.entry(id.to_string()).or_insert(role);
            }
        }
    }
}

fn derive_caller_label(record: &CallRecordModel) -> String {
    let candidates = [
        record.caller_name.as_ref(),
        record.from_number.as_ref(),
        record.caller_uri.as_ref(),
    ];

    candidates
        .iter()
        .filter_map(|value| value.as_ref())
        .map(|text| text.trim())
        .find(|text| !text.is_empty())
        .map(|text| text.to_string())
        .unwrap_or_else(|| "Caller".to_string())
}

fn derive_callee_label(record: &CallRecordModel) -> String {
    let candidates = [
        record.agent_name.as_ref(),
        record.to_number.as_ref(),
        record.callee_uri.as_ref(),
    ];

    candidates
        .iter()
        .filter_map(|value| value.as_ref())
        .map(|text| text.trim())
        .find(|text| !text.is_empty())
        .map(|text| text.to_string())
        .unwrap_or_else(|| "Callee".to_string())
}

fn build_sip_flow_columns(context: &SipFlowContext) -> Value {
    Value::Array(vec![
        json!({ "id": "time", "label": "Time" }),
        json!({ "id": "caller", "label": context.caller_label.clone() }),
        json!({ "id": "pbx", "label": context.pbx_label.clone() }),
        json!({ "id": "callee", "label": context.callee_label.clone() }),
    ])
}

fn determine_lanes(role: &str, direction: &SipFlowDirection) -> (&'static str, &'static str) {
    match role {
        "primary" => match direction {
            SipFlowDirection::Incoming => ("caller", "pbx"),
            SipFlowDirection::Outgoing => ("pbx", "caller"),
        },
        "b2bua" => match direction {
            SipFlowDirection::Incoming => ("callee", "pbx"),
            SipFlowDirection::Outgoing => ("pbx", "callee"),
        },
        _ => match direction {
            SipFlowDirection::Incoming => ("caller", "pbx"),
            SipFlowDirection::Outgoing => ("pbx", "callee"),
        },
    }
}

fn lane_label<'a>(lane: &str, context: &'a SipFlowContext) -> &'a str {
    match lane {
        "caller" => context.caller_label.as_str(),
        "pbx" => context.pbx_label.as_str(),
        "callee" => context.callee_label.as_str(),
        _ => context.pbx_label.as_str(),
    }
}

fn lane_index(lane: &str) -> i32 {
    match lane {
        "caller" => 0,
        "pbx" => 1,
        "callee" => 2,
        _ => 1,
    }
}

fn friendly_leg_label(leg_id: &str) -> String {
    if leg_id.len() <= 8 {
        leg_id.to_string()
    } else {
        format!("{}...", &leg_id[..8])
    }
}

fn friendly_leg_role_label(role: &str) -> &'static str {
    match role {
        "primary" => "Caller leg",
        "b2bua" => "Callee leg",
        _ => "Signaling leg",
    }
}

struct CdrData {
    record: CallRecord,
    raw_content: String,
    sip_flow_paths: Vec<SipFlowFileRef>,
    cdr_path: String,
    storage: Option<CdrStorage>,
}

fn build_record_payload(
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

    let quality = if record.quality_mos.is_some()
        || record.quality_latency_ms.is_some()
        || record.quality_jitter_ms.is_some()
        || record.quality_packet_loss_percent.is_some()
    {
        Some(json!({
            "mos": record.quality_mos,
            "latency_ms": record.quality_latency_ms,
            "jitter_ms": record.quality_jitter_ms,
            "packet_loss": record.quality_packet_loss_percent,
        }))
    } else {
        None
    };

    let caller_uri = record.caller_uri.clone();
    let callee_uri = record.callee_uri.clone();

    let recording = build_recording_payload(state, record, inline_recording_url);

    let rewrite_caller_original =
        json_lookup_nested_str(&record.metadata, &["rewrite", "caller_original"]);
    let rewrite_caller_final =
        json_lookup_nested_str(&record.metadata, &["rewrite", "caller_final"])
            .or_else(|| caller_uri.clone());
    let rewrite_callee_original =
        json_lookup_nested_str(&record.metadata, &["rewrite", "callee_original"]);
    let rewrite_callee_final =
        json_lookup_nested_str(&record.metadata, &["rewrite", "callee_final"])
            .or_else(|| callee_uri.clone());
    let rewrite_contact = json_lookup_nested_str(&record.metadata, &["rewrite", "contact"]);
    let rewrite_destination = json_lookup_nested_str(&record.metadata, &["rewrite", "destination"]);
    let billing = build_billing_payload(record);

    json!({
        "id": record.id,
        "call_id": record.call_id,
        "display_id": record.display_id,
        "direction": record.direction,
        "status": record.status,
        "from": record.from_number,
        "to": record.to_number,
        "cnam": record.caller_name,
        "agent": record.agent_name,
        "agent_extension": extension_number,
        "department": department_name,
        "queue": record.queue,
        "caller_uri": caller_uri,
        "callee_uri": callee_uri,
        "sip_gateway": sip_gateway,
        "sip_trunk": sip_trunk_name,
        "tags": tags,
        "has_transcript": record.has_transcript,
        "transcript_status": record.transcript_status,
        "transcript_language": record.transcript_language,
        "duration_secs": record.duration_secs,
        "recording": recording,
        "quality": quality,
        "started_at": record.started_at.to_rfc3339(),
        "ended_at": record.ended_at.map(|dt| dt.to_rfc3339()),
        "detail_url": state.url_for(&format!("/call-records/{}", record.id)),
        "billing": billing,
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

fn build_recording_payload(
    state: &ConsoleState,
    record: &CallRecordModel,
    inline_recording_url: Option<&str>,
) -> Option<Value> {
    let raw = record
        .recording_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());

    let url = if let Some(raw_value) = raw {
        if raw_value.starts_with("http://") || raw_value.starts_with("https://") {
            raw_value.to_string()
        } else {
            state.url_for(&format!("/call-records/{}/recording", record.id))
        }
    } else if let Some(fallback) = inline_recording_url {
        fallback.to_string()
    } else {
        return None;
    };

    Some(json!({
        "url": url,
        "duration_secs": record.recording_duration_secs,
    }))
}

fn derive_recording_download_url(state: &ConsoleState, record: &CallRecordModel) -> Option<String> {
    let raw = record
        .recording_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())?;

    if raw.starts_with("http://") || raw.starts_with("https://") {
        Some(raw.to_string())
    } else {
        Some(state.url_for(&format!("/call-records/{}/recording", record.id)))
    }
}

async fn load_cdr_data(state: &ConsoleState, record: &CallRecordModel) -> Option<CdrData> {
    let storage = resolve_cdr_storage(state);

    let mut candidates: Vec<String> = Vec::new();
    if let Some(path) = extract_cdr_path_from_metadata(&record.metadata) {
        candidates.push(path);
    }

    if candidates.is_empty() {
        candidates.push(default_cdr_file_name_for_model(record));
    }

    for candidate in candidates {
        let mut content: Option<String> = None;

        if let Some(ref storage_ref) = storage {
            match storage_ref.read_to_string(&candidate).await {
                Ok(value) => content = Some(value),
                Err(err) => {
                    warn!(call_id = %record.call_id, path = %candidate, "failed to load CDR from storage: {}", err);
                }
            }
        }

        if content.is_none() {
            match tokio::fs::read_to_string(&candidate).await {
                Ok(value) => content = Some(value),
                Err(err) => {
                    warn!(call_id = %record.call_id, path = %candidate, "failed to read CDR file: {}", err);
                }
            }
        }

        if let Some(raw) = content {
            match serde_json::from_str::<CallRecord>(&raw) {
                Ok(parsed) => {
                    let sip_flow_paths = extract_sip_flow_paths_from_cdr(&parsed);
                    return Some(CdrData {
                        record: parsed,
                        raw_content: raw,
                        sip_flow_paths,
                        cdr_path: candidate,
                        storage: storage.clone(),
                    });
                }
                Err(err) => {
                    warn!(call_id = %record.call_id, path = %candidate, "failed to parse CDR file: {}", err);
                }
            }
        }
    }

    None
}

async fn load_stored_transcript(
    record: &CallRecordModel,
    cdr: Option<&CdrData>,
) -> AnyResult<Option<StoredTranscript>> {
    let mut candidates: Vec<String> = Vec::new();

    if let Some(path) = transcript_file_from_metadata(&record.metadata) {
        candidates.push(path);
    }

    if let Some(cdr_data) = cdr {
        if let Some(path) = transcript_file_from_cdr(&cdr_data.record) {
            candidates.push(path);
        } else {
            let mut fallback = PathBuf::from(&cdr_data.cdr_path);
            fallback.set_extension("transcript.json");
            candidates.push(fallback.to_string_lossy().into_owned());
        }
    }

    candidates.retain(|candidate| !candidate.trim().is_empty());
    candidates.dedup();

    let storage = cdr.and_then(|data| data.storage.as_ref());

    for candidate in candidates {
        let path = candidate.trim();
        match read_transcript_file(storage, path).await {
            Ok(mut transcript) => {
                if transcript.source.trim().is_empty() {
                    transcript.source = "sensevoice-cli".to_string();
                }
                return Ok(Some(transcript));
            }
            Err(err) => {
                warn!(call_id = %record.call_id, file = %path, "failed to read transcript file: {}", err);
            }
        }
    }

    Ok(None)
}

fn resolve_models_path(cfg: &TranscriptConfig) -> Option<String> {
    if let Ok(env_path) = std::env::var("MODEL_PATH") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    cfg.models_path
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

async fn read_transcript_file(
    storage: Option<&CdrStorage>,
    path: &str,
) -> AnyResult<StoredTranscript> {
    if let Some(storage_ref) = storage {
        if let Ok(content) = storage_ref.read_to_string(path).await {
            let transcript: StoredTranscript = serde_json::from_str(&content)
                .with_context(|| format!("parse transcript file {}", path))?;
            return Ok(transcript);
        }
    }

    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read transcript file {}", path))?;
    let transcript: StoredTranscript = serde_json::from_str(&content)
        .with_context(|| format!("parse transcript file {}", path))?;
    Ok(transcript)
}

fn transcript_file_from_cdr(record: &CallRecord) -> Option<String> {
    record
        .extras
        .as_ref()
        .and_then(|extras| extras.get("transcript_file"))
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn transcript_file_from_metadata(metadata: &Option<Value>) -> Option<String> {
    metadata
        .as_ref()
        .and_then(|value| value.get("transcript"))
        .and_then(|value| value.get("file"))
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn extract_cdr_path_from_metadata(metadata: &Option<Value>) -> Option<String> {
    metadata
        .as_ref()
        .and_then(|value| value.get("cdr_file"))
        .and_then(|value| value.as_str())
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

fn default_cdr_file_name_for_model(record: &CallRecordModel) -> String {
    format!(
        "{}_{}.json",
        record.started_at.format("%Y%m%d-%H%M%S"),
        record.call_id
    )
}

fn extract_sip_flow_paths_from_cdr(record: &CallRecord) -> Vec<SipFlowFileRef> {
    let mut paths = Vec::new();
    if let Some(extras) = record.extras.as_ref() {
        if let Some(value) = extras.get("sip_flow_files") {
            if let Some(entries) = value.as_array() {
                for entry in entries {
                    if let Some(path) = entry.get("path").and_then(|v| v.as_str()) {
                        let leg = entry
                            .get("leg")
                            .and_then(|v| v.as_str())
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty());
                        let leg_role = entry
                            .get("role")
                            .and_then(|v| v.as_str())
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty());
                        let trimmed = path.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        paths.push(SipFlowFileRef {
                            leg: leg.clone(),
                            leg_role: leg_role.clone(),
                            path: trimmed.to_string(),
                        });
                    }
                }
            }
        }
    }
    paths
}

fn build_transcript_payload_value(
    record: &CallRecordModel,
    transcript: Option<&StoredTranscript>,
) -> Value {
    if let Some(data) = transcript {
        let segments: Vec<Value> = data
            .segments
            .iter()
            .map(|segment| {
                let speaker = segment
                    .channel
                    .map(|ch| format!("Channel {}", ch + 1))
                    .unwrap_or_else(|| "Speaker".to_string());
                json!({
                    "idx": segment.idx,
                    "text": segment.text,
                    "start": segment.start,
                    "end": segment.end,
                    "channel": segment.channel,
                    "speaker": speaker,
                })
            })
            .collect();

        json!({
            "available": true,
            "status": record.transcript_status,
            "language": data
                .language
                .clone()
                .or(record.transcript_language.clone()),
            "generated_at": data.generated_at.to_rfc3339(),
            "duration_secs": data.duration_secs,
            "text": data.text,
            "segments": segments,
            "source": data.source,
            "analysis": data.analysis,
        })
    } else {
        json!({
            "available": record.has_transcript,
            "status": record.transcript_status,
            "language": record.transcript_language,
            "generated_at": record.updated_at.to_rfc3339(),
            "duration_secs": Value::Null,
            "text": "",
            "segments": Value::Array(vec![]),
            "source": Value::Null,
            "analysis": Value::Null,
        })
    }
}

fn transcript_excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return String::new();
    }
    format!("{}…", &text[..end])
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

fn select_recording_path(record: &CallRecordModel, cdr: Option<&CdrData>) -> Option<String> {
    if let Some(cdr_data) = cdr {
        for media in &cdr_data.record.recorder {
            let path = media.path.trim();
            if path.is_empty() {
                continue;
            }
            if Path::new(path).exists() {
                return Some(path.to_string());
            }
        }
    }

    if let Some(url) = record.recording_url.as_ref() {
        let trimmed = url.trim();
        if !trimmed.is_empty() && Path::new(trimmed).exists() {
            return Some(trimmed.to_string());
        }
    }

    None
}

async fn update_cdr_with_transcript(
    cdr: Option<&CdrData>,
    transcript: &StoredTranscript,
    path: &str,
    excerpt: &str,
) {
    let Some(cdr_data) = cdr else {
        return;
    };

    let mut record = cdr_data.record.clone();
    let mut extras = record.extras.unwrap_or_default();
    extras.insert(
        "transcript_file".to_string(),
        Value::String(path.to_string()),
    );
    extras.insert(
        "transcript_generated_at".to_string(),
        Value::String(transcript.generated_at.to_rfc3339()),
    );
    if let Some(language) = transcript.language.as_ref() {
        extras.insert(
            "transcript_language".to_string(),
            Value::String(language.clone()),
        );
    }
    extras.insert(
        "transcript_excerpt".to_string(),
        Value::String(excerpt.to_string()),
    );
    record.extras = Some(extras);

    match serde_json::to_string_pretty(&record) {
        Ok(serialized) => {
            if let Some(storage_ref) = cdr_data.storage.as_ref() {
                if let Err(err) = storage_ref
                    .write_bytes(&cdr_data.cdr_path, serialized.as_bytes())
                    .await
                {
                    warn!(file = %cdr_data.cdr_path, "failed to write CDR with transcript metadata: {}", err);
                }
            } else if let Err(err) = tokio::fs::write(&cdr_data.cdr_path, serialized).await {
                warn!(file = %cdr_data.cdr_path, "failed to write CDR with transcript metadata: {}", err);
            }
        }
        Err(err) => {
            warn!(file = %cdr_data.cdr_path, "failed to serialize CDR with transcript metadata: {}", err);
        }
    }
}

async fn load_sip_flow_from_files(
    storage: Option<&CdrStorage>,
    paths: &[SipFlowFileRef],
) -> Vec<LeggedSipMessage> {
    let mut all_items = Vec::new();
    for entry in paths {
        let mut items = read_sip_flow_file(storage, &entry.path).await;
        let leg = entry.leg.clone();
        let leg_role = entry.leg_role.clone();
        for item in items.drain(..) {
            all_items.push(LeggedSipMessage {
                leg: leg.clone(),
                leg_role: leg_role.clone(),
                item,
            });
        }
    }
    if all_items.len() > 1 {
        all_items.sort_by(|a, b| a.item.timestamp.cmp(&b.item.timestamp));
    }
    all_items
}

fn build_billing_payload(record: &CallRecordModel) -> Value {
    let billable_secs = record.billing_billable_secs.unwrap_or(0);
    let billable_minutes = if billable_secs > 0 {
        Some((billable_secs as f64 / 60.0 * 100.0).round() / 100.0)
    } else {
        None
    };

    json!({
        "status": record.billing_status.clone(),
        "method": record.billing_method.clone(),
        "currency": record.billing_currency.clone(),
        "billable_secs": record.billing_billable_secs,
        "billable_minutes": billable_minutes,
        "rate_per_minute": record.billing_rate_per_minute,
        "amount": {
            "subtotal": record.billing_amount_subtotal,
            "tax": record.billing_amount_tax,
            "total": record.billing_amount_total,
        },
        "result": record.billing_result.clone().unwrap_or(Value::Null),
        "snapshot_available": record.billing_snapshot.is_some(),
    })
}

fn build_detail_payload(
    record: &CallRecordModel,
    related: &RelatedContext,
    state: &ConsoleState,
    mut sip_flow: Vec<LeggedSipMessage>,
    cdr: Option<&CdrData>,
    transcript: Option<&StoredTranscript>,
) -> Value {
    let inline_recording_url = select_recording_path(record, cdr)
        .map(|_| state.url_for(&format!("/call-records/{}/recording", record.id)));
    let record_payload =
        build_record_payload(record, related, state, inline_recording_url.as_deref());
    let participants = build_participants(record, related);
    let billing_summary = record_payload
        .get("billing")
        .cloned()
        .unwrap_or_else(|| build_billing_payload(record));
    let transcript_payload = build_transcript_payload_value(record, transcript);
    let transcript_preview = transcript
        .map(|data| {
            json!({
                "excerpt": transcript_excerpt(&data.text, 360),
                "language": data.language,
                "generated_at": data.generated_at.to_rfc3339(),
            })
        })
        .unwrap_or(Value::Null);

    let transcript_capabilities = {
        let mut configured = false;
        let mut command: Option<String> = None;
        let mut command_exists_flag = false;
        let mut models_path: Option<String> = None;
        let mut model_exists_flag = false;
        let mut issues: Vec<String> = Vec::new();

        if let Some(app) = state.app_state() {
            if let Some(proxy_cfg) = app.config.proxy.as_ref() {
                if let Some(transcript_cfg) = proxy_cfg.transcript.as_ref() {
                    configured = true;
                    let resolved_command = transcript_cfg
                        .command
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .unwrap_or("sensevoice-cli");
                    command = Some(resolved_command.to_string());
                    command_exists_flag = command_exists(resolved_command);

                    if let Some(path) = resolve_models_path(transcript_cfg) {
                        models_path = Some(path.clone());
                        let model_path = model_file_path(&path);
                        model_exists_flag = model_path.exists();
                        if !model_exists_flag {
                            issues.push(format!(
                                "SenseVoice model not found at {}.",
                                model_path.display()
                            ));
                        }
                    } else {
                        issues.push("SenseVoice models_path is not configured.".to_string());
                    }

                    if !command_exists_flag {
                        issues.push("sensevoice-cli binary not found on server PATH.".to_string());
                    }
                } else {
                    issues.push("SenseVoice CLI transcription is not configured.".to_string());
                }
            } else {
                issues.push("Proxy configuration unavailable.".to_string());
            }
        } else {
            issues.push("Application state unavailable.".to_string());
        }

        json!({
            "sensevoice_cli": {
                "configured": configured,
                "command": command,
                "command_exists": command_exists_flag,
                "models_path": models_path,
                "model_exists": model_exists_flag,
                "ready": configured && command_exists_flag && model_exists_flag,
                "missing": issues,
            }
        })
    };

    let media_metrics = json!({
        "audio_codec": json_lookup_str(&record.metadata, "audio_codec"),
        "rtp_packets": json_lookup_number(&record.analytics, "rtp_packets"),
        "avg_jitter_ms": record.quality_jitter_ms,
        "packet_loss_percent": record.quality_packet_loss_percent,
        "mos": record.quality_mos,
        "rtcp_observations": Value::Array(vec![]),
    });

    let signaling = if let Some(data) = cdr {
        let value = build_signaling_from_cdr(data);
        if value.is_null() {
            record.signaling.clone().unwrap_or(Value::Null)
        } else {
            value
        }
    } else {
        record.signaling.clone().unwrap_or(Value::Null)
    };
    let rewrite = record_payload
        .get("rewrite")
        .cloned()
        .unwrap_or(Value::Null);

    let leg_roles = resolve_leg_roles(record, cdr);
    for entry in sip_flow.iter_mut() {
        if entry.leg_role.is_none() {
            if let Some(ref leg_id) = entry.leg {
                if let Some(role) = leg_roles.get(leg_id) {
                    entry.leg_role = Some(role.clone());
                } else if leg_id == &record.call_id {
                    entry.leg_role = Some("primary".to_string());
                }
            } else {
                entry.leg_role = Some("primary".to_string());
            }
        }
    }

    let flow_context = SipFlowContext {
        caller_label: derive_caller_label(record),
        pbx_label: "PBX".to_string(),
        callee_label: derive_callee_label(record),
    };

    let sip_flow_entries = build_sip_flow_entries(sip_flow, &flow_context);
    let sip_flow_columns = build_sip_flow_columns(&flow_context);

    let mut sip_flow_map = JsonMap::new();
    sip_flow_map.insert("columns".to_string(), sip_flow_columns);
    sip_flow_map.insert("entries".to_string(), Value::Array(sip_flow_entries));
    let sip_flow_value = Value::Object(sip_flow_map);

    let (metadata_download, sip_flow_download) = if let Some(data) = cdr {
        let metadata_url = state.url_for(&format!(
            "/call-records/{}/metadata?path={}",
            record.id,
            encode(&data.cdr_path)
        ));
        let flow = data
            .sip_flow_paths
            .first()
            .map(|entry| {
                Value::String(state.url_for(&format!(
                    "/call-records/{}/sip-flow?path={}",
                    record.id,
                    encode(&entry.path)
                )))
            })
            .unwrap_or(Value::Null);
        (Value::String(metadata_url), flow)
    } else {
        (Value::Null, Value::Null)
    };

    let mut download_recording = derive_recording_download_url(state, record);
    if download_recording.is_none() {
        download_recording = inline_recording_url.clone();
    }

    json!({
        "back_url": state.url_for("/call-records"),
        "record": record_payload,
        "sip_flow": sip_flow_value,
        "media_metrics": media_metrics,
        "transcript": transcript_payload,
        "transcript_preview": transcript_preview,
        "notes": build_console_note_payload(&record.metadata),
        "participants": participants,
        "signaling": signaling,
        "rewrite": rewrite,
        "billing": json!({
            "summary": billing_summary,
            "snapshot": record.billing_snapshot.clone().unwrap_or(Value::Null),
            "result": record.billing_result.clone().unwrap_or(Value::Null),
        }),
        "transcript_capabilities": transcript_capabilities,
        "actions": json!({
            "download_recording": download_recording,
            "download_metadata": metadata_download,
            "download_sip_flow": sip_flow_download,
            "transcript_url": state.url_for(&format!("/call-records/{}/transcript", record.id)),
            "update_record": state.url_for(&format!("/call-records/{}", record.id)),
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
        "is_b2bua": cdr.record.refer_callrecord.is_some(),
        "legs": legs,
    })
}

fn append_cdr_leg(legs: &mut Vec<Value>, role: &str, record: &CallRecord) {
    legs.push(signaling_leg_payload(role, record));
    if let Some(refer) = record.refer_callrecord.as_ref() {
        append_cdr_leg(legs, "b2bua", refer);
    }
}

fn signaling_leg_payload(role: &str, record: &CallRecord) -> Value {
    json!({
        "role": role,
        "call_type": record.call_type,
        "call_id": record.call_id,
        "caller": record.caller,
        "callee": record.callee,
        "status_code": record.status_code,
        "hangup_reason": record
            .hangup_reason
            .as_ref()
            .map(|reason| reason.to_string()),
        "start_time": record.start_time,
        "ring_time": record.ring_time,
        "answer_time": record.answer_time,
        "end_time": record.end_time,
        "offer": record.offer,
        "answer": record.answer,
    })
}

async fn load_sip_flow_from_metadata(record: &CallRecordModel) -> Vec<LeggedSipMessage> {
    let paths = extract_sip_flow_paths(record);
    if paths.is_empty() {
        return Vec::new();
    }

    load_sip_flow_from_files(None, &paths).await
}

fn extract_sip_flow_paths(record: &CallRecordModel) -> Vec<SipFlowFileRef> {
    let mut paths = Vec::new();
    if let Some(metadata) = record.metadata.as_ref() {
        if let Some(entries) = metadata
            .get("sip_flow_files")
            .and_then(|value| value.as_array())
        {
            for entry in entries {
                if let Some(path) = entry.get("path").and_then(|v| v.as_str()) {
                    let leg = entry
                        .get("leg")
                        .and_then(|v| v.as_str())
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty());
                    let leg_role = entry
                        .get("role")
                        .and_then(|v| v.as_str())
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty());
                    let trimmed = path.trim();
                    if !trimmed.is_empty() {
                        paths.push(SipFlowFileRef {
                            leg: leg.clone(),
                            leg_role: leg_role.clone(),
                            path: trimmed.to_string(),
                        });
                    }
                }
            }
        }
    }
    paths
}

async fn read_sip_flow_file(storage: Option<&CdrStorage>, path: &str) -> Vec<SipMessageItem> {
    if let Some(storage_ref) = storage {
        if let Ok(bytes) = storage_ref.read_bytes(path).await {
            return parse_sip_flow_bytes(path, &bytes);
        }
    }

    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(err) => {
            warn!("failed to open sip flow file '{}': {}", path, err);
            return Vec::new();
        }
    };

    let mut items = Vec::new();
    let mut reader = BufReader::new(file).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<SipMessageItem>(trimmed) {
            Ok(item) => items.push(item),
            Err(err) => {
                warn!("failed to parse sip flow entry from '{}': {}", path, err);
            }
        }
    }

    items
}

fn parse_sip_flow_bytes(path: &str, bytes: &[u8]) -> Vec<SipMessageItem> {
    let mut items = Vec::new();
    let content = String::from_utf8_lossy(bytes);
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<SipMessageItem>(trimmed) {
            Ok(item) => items.push(item),
            Err(err) => {
                warn!("failed to parse sip flow entry from '{}': {}", path, err);
            }
        }
    }
    items
}

fn build_sip_flow_entries(items: Vec<LeggedSipMessage>, context: &SipFlowContext) -> Vec<Value> {
    if items.is_empty() {
        return Vec::new();
    }

    let first_ts = items[0].item.timestamp;
    items
        .into_iter()
        .enumerate()
        .map(|(sequence, entry)| {
            let LeggedSipMessage {
                leg,
                leg_role,
                item,
            } = entry;

            let offset_ms = item
                .timestamp
                .signed_duration_since(first_ts)
                .num_milliseconds();
            let offset = if offset_ms == 0 {
                "0 ms".to_string()
            } else if offset_ms > 0 {
                format!("+{} ms", offset_ms)
            } else {
                format!("{} ms", offset_ms)
            };

            let raw_first_line = item
                .content
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();

            let mut method: Option<String> = None;
            let mut status_code: Option<u16> = None;
            let mut summary = raw_first_line.clone();

            if raw_first_line.to_ascii_uppercase().starts_with("SIP/2.0") {
                let rest = raw_first_line[7..].trim();
                let mut parts = rest.splitn(2, ' ');
                if let Some(code_part) = parts.next() {
                    if let Ok(code) = code_part.trim().parse::<u16>() {
                        status_code = Some(code);
                        method = Some(code.to_string());
                    }
                }
                if let Some(reason) = parts.next() {
                    summary = reason.trim().to_string();
                } else {
                    summary = "Response".to_string();
                }
            } else {
                let mut parts = raw_first_line.split_whitespace();
                if let Some(m) = parts.next() {
                    method = Some(m.to_string());
                    let rest = parts.collect::<Vec<_>>().join(" ");
                    if !rest.is_empty() {
                        summary = rest;
                    }
                }
            }

            let direction = match item.direction {
                SipFlowDirection::Incoming => "Incoming",
                SipFlowDirection::Outgoing => "Outgoing",
            };

            let role_value = leg_role
                .unwrap_or_else(|| "primary".to_string())
                .to_lowercase();
            let (lane_from, lane_to) = determine_lanes(&role_value, &item.direction);
            let flow_direction = if lane_to == "pbx" {
                "inbound"
            } else if lane_from == "pbx" {
                "outbound"
            } else {
                "transit"
            };
            let arrow_direction = if lane_index(lane_from) <= lane_index(lane_to) {
                "right"
            } else {
                "left"
            };
            let lane_display = if lane_from == "pbx" {
                lane_to.to_string()
            } else {
                lane_from.to_string()
            };

            let mut object = JsonMap::new();
            object.insert("sequence".to_string(), Value::from(sequence as i64));
            object.insert(
                "timestamp".to_string(),
                Value::String(item.timestamp.to_rfc3339()),
            );
            object.insert("offset".to_string(), Value::String(offset));
            object.insert(
                "direction".to_string(),
                Value::String(direction.to_string()),
            );
            object.insert("summary".to_string(), Value::String(summary));
            if let Some(method_value) = method {
                object.insert("method".to_string(), Value::String(method_value));
            }
            if let Some(status) = status_code {
                object.insert("status_code".to_string(), Value::from(status));
            }
            object.insert("raw".to_string(), Value::String(item.content));
            object.insert(
                "lane_from".to_string(),
                Value::String(lane_from.to_string()),
            );
            object.insert("lane_to".to_string(), Value::String(lane_to.to_string()));
            object.insert(
                "from_label".to_string(),
                Value::String(lane_label(lane_from, context).to_string()),
            );
            object.insert(
                "to_label".to_string(),
                Value::String(lane_label(lane_to, context).to_string()),
            );
            object.insert(
                "flow_direction".to_string(),
                Value::String(flow_direction.to_string()),
            );
            object.insert(
                "arrow".to_string(),
                Value::String(arrow_direction.to_string()),
            );
            object.insert("lane_display".to_string(), Value::String(lane_display));

            if let Some(leg_id) = leg {
                let trimmed = leg_id.trim();
                if !trimmed.is_empty() {
                    object.insert("leg".to_string(), Value::String(trimmed.to_string()));
                    object.insert(
                        "leg_label".to_string(),
                        Value::String(friendly_leg_label(trimmed)),
                    );
                }
            }

            if !role_value.is_empty() {
                object.insert("leg_role".to_string(), Value::String(role_value.clone()));
                object.insert(
                    "leg_role_label".to_string(),
                    Value::String(friendly_leg_role_label(&role_value).to_string()),
                );
            }

            Value::Object(object)
        })
        .collect()
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ConsoleNote {
    #[serde(default)]
    text: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    updated_by: Option<String>,
}

fn extract_console_note(metadata: &Option<Value>) -> Option<ConsoleNote> {
    let value = metadata
        .as_ref()
        .and_then(|value| value.as_object())
        .and_then(|map| map.get("console_note"))?
        .clone();
    let mut note: ConsoleNote = serde_json::from_value(value).ok()?;
    note.text = note.text.trim().to_string();
    Some(note)
}

fn build_console_note_payload(metadata: &Option<Value>) -> Value {
    match extract_console_note(metadata) {
        Some(note) => json!({
            "text": note.text,
            "updated_at": note.updated_at,
            "updated_by": note.updated_by,
        }),
        None => Value::Null,
    }
}

fn json_lookup_str(source: &Option<Value>, key: &str) -> Option<String> {
    source
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn json_lookup_number(source: &Option<Value>, key: &str) -> Option<f64> {
    source
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_f64())
}

fn json_lookup_nested_str(source: &Option<Value>, path: &[&str]) -> Option<String> {
    let mut current = source.as_ref()?;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(|value| value.to_string())
}

async fn build_summary(db: &DatabaseConnection, condition: Condition) -> Result<Value, DbErr> {
    let total = CallRecordEntity::find()
        .filter(condition.clone())
        .count(db)
        .await?;

    let answered = CallRecordEntity::find()
        .filter(condition.clone())
        .filter(CallRecordColumn::Status.eq("completed"))
        .count(db)
        .await?;

    let missed = CallRecordEntity::find()
        .filter(condition.clone())
        .filter(CallRecordColumn::Status.eq("missed"))
        .count(db)
        .await?;

    let failed = CallRecordEntity::find()
        .filter(condition.clone())
        .filter(CallRecordColumn::Status.eq("failed"))
        .count(db)
        .await?;

    let transcribed = CallRecordEntity::find()
        .filter(condition.clone())
        .filter(CallRecordColumn::HasTranscript.eq(true))
        .count(db)
        .await?;

    let total_duration_secs = CallRecordEntity::find()
        .select_only()
        .column_as(CallRecordColumn::DurationSecs.sum(), "total_duration")
        .filter(condition.clone())
        .into_tuple::<Option<i64>>()
        .one(db)
        .await?
        .flatten()
        .unwrap_or(0);

    let total_minutes = total_duration_secs as f64 / 60.0;
    let avg_duration = if total > 0 {
        total_duration_secs as f64 / total as f64
    } else {
        0.0
    };

    let unique_dids = CallRecordEntity::find()
        .select_only()
        .column_as(
            Expr::col(CallRecordColumn::ToNumber).count_distinct(),
            "unique_dids",
        )
        .filter(condition.clone())
        .into_tuple::<Option<i64>>()
        .one(db)
        .await?
        .flatten()
        .unwrap_or(0);

    let total_billable_secs = CallRecordEntity::find()
        .select_only()
        .column_as(
            CallRecordColumn::BillingBillableSecs.sum(),
            "billing_billable_secs",
        )
        .filter(condition.clone())
        .into_tuple::<Option<i64>>()
        .one(db)
        .await?
        .flatten()
        .unwrap_or(0);

    let billing_status_rows: Vec<(Option<String>, Option<i64>)> = CallRecordEntity::find()
        .select_only()
        .column(CallRecordColumn::BillingStatus)
        .column_as(
            Expr::col(CallRecordColumn::BillingStatus).count(),
            "status_count",
        )
        .filter(condition.clone())
        .group_by(CallRecordColumn::BillingStatus)
        .into_tuple::<(Option<String>, Option<i64>)>()
        .all(db)
        .await?;

    let mut billing_charged_calls = 0;
    let mut billing_included_calls = 0;
    let mut billing_zero_duration_calls = 0;
    let mut billing_unrated_calls = 0;

    for (status_opt, count_opt) in billing_status_rows {
        let count = count_opt.unwrap_or(0);
        match status_opt.as_deref() {
            Some(BILLING_STATUS_CHARGED) => billing_charged_calls = count,
            Some(BILLING_STATUS_INCLUDED) => billing_included_calls = count,
            Some(BILLING_STATUS_ZERO_DURATION) => billing_zero_duration_calls = count,
            Some(BILLING_STATUS_UNRATED) | None => billing_unrated_calls = count,
            _ => {}
        }
    }

    let revenue_rows: Vec<(Option<String>, Option<f64>)> = CallRecordEntity::find()
        .select_only()
        .column(CallRecordColumn::BillingCurrency)
        .column_as(
            CallRecordColumn::BillingAmountTotal.sum(),
            "billing_amount_total",
        )
        .filter(condition.clone())
        .group_by(CallRecordColumn::BillingCurrency)
        .into_tuple::<(Option<String>, Option<f64>)>()
        .all(db)
        .await?;

    let mut billing_revenue: Vec<Value> = Vec::new();
    for (currency_opt, amount_opt) in revenue_rows {
        if let Some(amount) = amount_opt {
            let mut currency = currency_opt.unwrap_or_default();
            if currency.trim().is_empty() {
                currency = "USD".to_string();
            }
            let rounded = (amount * 100.0).round() / 100.0;
            billing_revenue.push(json!({
                "currency": currency,
                "total": rounded,
            }));
        }
    }

    let billing_billable_minutes = (total_billable_secs as f64 / 60.0 * 100.0).round() / 100.0;
    let billing_rated_calls = billing_charged_calls + billing_included_calls;

    Ok(json!({
        "total": total,
        "answered": answered,
        "missed": missed,
        "failed": failed,
        "transcribed": transcribed,
        "avg_duration": (avg_duration * 10.0).round() / 10.0,
        "total_minutes": (total_minutes * 10.0).round() / 10.0,
        "unique_dids": unique_dids,
        "billing_billable_minutes": billing_billable_minutes,
        "billing_rated_calls": billing_rated_calls,
        "billing_charged_calls": billing_charged_calls,
        "billing_included_calls": billing_included_calls,
        "billing_zero_duration_calls": billing_zero_duration_calls,
        "billing_unrated_calls": billing_unrated_calls,
        "billing_revenue": billing_revenue,
    }))
}

fn equals_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConsoleConfig,
        console::ConsoleState,
        models::{call_record, migration::Migrator},
    };
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
        ConsoleState::initialize(
            db,
            ConsoleConfig {
                session_secret: "secret".into(),
                base_path: "/console".into(),
                allow_registration: false,
            },
        )
        .await
        .expect("console state")
    }

    #[tokio::test]
    async fn load_filters_returns_defaults() {
        let db = setup_db().await;
        let filters = load_filters(&db).await.expect("filters");
        assert!(filters.get("status").is_some());
        assert!(filters.get("direction").is_some());
        assert!(filters.get("billing_status").is_some());
    }

    #[tokio::test]
    async fn build_record_payload_contains_basic_fields() {
        let db = setup_db().await;
        let state = create_console_state(db.clone()).await;

        let record = call_record::ActiveModel {
            call_id: Set("call-1".into()),
            direction: Set("inbound".into()),
            status: Set("completed".into()),
            started_at: Set(Utc::now()),
            duration_secs: Set(60),
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
        let payload = build_record_payload(&record, &related, &state, None);
        assert_eq!(payload["id"], 1);
        assert!(payload.get("billing").is_some());
    }
}
