use crate::callrecord::CallRecord;
use crate::callrecord::storage;
use crate::callrecord::storage::CdrStorage;
use crate::console::{ConsoleState, handlers::forms, middleware::AuthRequired};
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
    routing::get,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sea_orm::sea_query::Order;
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
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tracing::warn;
use urlencoding::encode;

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
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct DownloadRequest {
    path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCallRecordPayload {
    #[serde(default)]
    tags: Option<Vec<String>>,
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
            "/call-records/{id}/metadata",
            get(download_call_record_metadata),
        )
        .route(
            "/call-records/{id}/sip-flow",
            get(download_call_record_sip_flow),
        )
        .route("/call-records/{id}/recording", get(stream_call_recording))
}

async fn download_call_record_sip_flow(
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
            warn!(id = pk, "failed to load call record for sip flow: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "message": "Failed to load call record" })),
            )
                .into_response();
        }
    };

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
    // Default main call_id to "primary" if not overridden
    call_id_roles.insert(record.call_id.clone(), "primary".to_string());

    let cdr_data = load_cdr_data(&state, &record).await;
    if let Some(cdr) = &cdr_data {
        for (cid, role) in &cdr.record.sip_leg_roles {
            call_id_roles.insert(cid.clone(), role.clone());
        }
    }

    let mut flow_items = Vec::new();
    // Map of (Role, Src, Dst) -> PacketCount
    let mut rtp_stats: HashMap<(String, String, String), usize> = HashMap::new();

    for (cid, role) in &call_id_roles {
        match backend.query_flow(cid, start_time, end_time).await {
            Ok(items) => {
                for item in items {
                    flow_items.push((item, role.clone(), cid.clone()));
                }
            }
            Err(err) => {
                warn!(id = pk, call_id = %cid, "failed to query sip flow for leg: {}", err);
            }
        }

        match backend.query_media_stats(cid, start_time, end_time).await {
            Ok(stats) => {
                for (leg, src_addr, count) in stats {
                    *rtp_stats
                        .entry((
                            role.clone(),
                            if src_addr.is_empty() {
                                format!("Leg {}", leg)
                            } else {
                                src_addr
                            },
                            "RTP".to_string(),
                        ))
                        .or_insert(0) += count;
                }
            }
            Err(err) => {
                warn!(id = pk, call_id = %cid, "failed to query media stats for leg: {}", err);
            }
        }
    }

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

    let rtp_streams: Vec<Value> = rtp_stats
        .into_iter()
        .map(|((role, src, dst), count)| {
            json!({
                "role": role,
                "src_addr": src,
                "dst_addr": dst,
                "packet_count": count
            })
        })
        .collect();

    Json(json!({
        "call_id": record.call_id,
        "start_time": record.started_at,
        "status": "success",
        "flow": flow_json,
        "rtp_streams": rtp_streams,
    }))
    .into_response()
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
        Some(path) => Some(path),
        None => None,
    };

    // Try to stream from file first
    if let Some(ref path) = recording_path {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            if meta.is_file() && meta.len() > 0 {
                return stream_file_with_range(path, meta.len(), &headers).await;
            }
        }
    }

    // Fallback: Try to get recording from sipflow backend
    if let Some(server) = state.sip_server() {
        if let Some(sipflow) = &server.sip_flow {
            if let Some(backend) = sipflow.backend() {
                let call_time = record.created_at;
                let start_time =
                    (call_time - chrono::Duration::hours(1)).with_timezone(&chrono::Local);
                let end_time =
                    (call_time + chrono::Duration::hours(2)).with_timezone(&chrono::Local);

                if let Ok(audio_data) = backend
                    .query_media(&record.call_id, start_time, end_time)
                    .await
                {
                    if !audio_data.is_empty() {
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header(http::header::CONTENT_TYPE, "audio/wav")
                            .header(http::header::CONTENT_LENGTH, audio_data.len())
                            .header(
                                http::header::CONTENT_DISPOSITION,
                                "inline; filename=\"recording.wav\"",
                            )
                            .body(Body::from(audio_data))
                            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                    }
                }
            }
        }
    }

    (
        StatusCode::NOT_FOUND,
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

async fn page_call_record_detail(
    AxumPath(id_param): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
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

    let related = match load_related_context(db, &[model.clone()]).await {
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
    let payload = build_detail_payload(&model, &related, &state, cdr_data.as_ref());

    state.render(
        "console/call_record_detail.html",
        json!({
            "nav_active": "call-records",
            "page_title": format!("Call record · {}", model.id),
            "call_id": model.call_id,
            "call_data": serde_json::to_string(&payload).unwrap_or_default(),

            "addon_scripts": state.get_injected_scripts(&format!("/console/call-records/{}", model.id)),
        }),
    )
}

async fn update_call_record(
    AxumPath(pk): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
    Json(payload): Json<UpdateCallRecordPayload>,
) -> Response {
    if payload.tags.is_none() {
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
    if !changed {
        let response = json!({
            "status": "noop",
            "record": {
                "id": record.id,
                "tags": extract_tags(&record.tags),
            },
            "notes": Value::Null,
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
        "notes": Value::Null,
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

    Ok(json!({
        "status": ["any", "completed", "missed", "failed"],
        "direction": ["any", "inbound", "outbound", "internal"],
        "departments": departments,
        "sip_trunks": sip_trunks,
        "tags": [],
    }))
}

fn build_condition(filters: &Option<QueryCallRecordFilters>) -> Condition {
    let mut condition = Condition::all();

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

    let caller_uri = record.caller_uri.clone();
    let callee_uri = record.callee_uri.clone();

    let recording = build_recording_payload(state, record, inline_recording_url);

    let rewrite_caller_original = record.rewrite_original_from.clone();
    let rewrite_caller_final = caller_uri.clone();
    let rewrite_callee_original = record.rewrite_original_to.clone();
    let rewrite_callee_final = callee_uri.clone();
    let rewrite_contact = Option::<String>::None;
    let rewrite_destination = Option::<String>::None;
    let status_code = Option::<u16>::None;
    let ring_time = Option::<String>::None;
    let answer_time = Option::<String>::None;
    let hangup_reason = Option::<String>::None;
    let hangup_messages = Vec::<Value>::new();

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
        "started_at": record.started_at.to_rfc3339(),
        "ring_time": ring_time,
        "answer_time": answer_time,
        "ended_at": record.ended_at.map(|dt| dt.to_rfc3339()),
        "detail_url": state.url_for(&format!("/call-records/{}", record.id)),
        "status_code": status_code,
        "hangup_reason": hangup_reason,
        "hangup_messages": hangup_messages,
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
    } else if state
        .sip_server()
        .map(|server| server.sip_flow.is_some())
        .unwrap_or(false)
    {
        // If sipflow is enabled, allow trying to play/download the recording
        state.url_for(&format!("/call-records/{}/recording", record.id))
    } else {
        return None;
    };

    Some(json!({
        "url": url,
        "duration_secs": record.recording_duration_secs,
    }))
}

fn derive_recording_download_url(state: &ConsoleState, record: &CallRecordModel) -> Option<String> {
    if let Some(raw) = record
        .recording_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return Some(raw.to_string());
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

fn strip_storage_root(state: &ConsoleState, path: &str) -> String {
    if let Some(app) = state.app_state() {
        if let Some(config) = &app.config().callrecord {
            match config {
                crate::config::CallRecordConfig::Local { root } => {
                    let root_path = Path::new(root);
                    let candidate_path = Path::new(path);
                    if let Ok(stripped) = candidate_path.strip_prefix(root_path) {
                        stripped.to_string_lossy().to_string()
                    } else {
                        let root_str = root.trim_end_matches('/');
                        if path.starts_with(root_str) {
                            path[root_str.len()..].trim_start_matches('/').to_string()
                        } else {
                            path.to_string()
                        }
                    }
                }
                crate::config::CallRecordConfig::S3 { root, .. } => {
                    let root_str = root.trim_end_matches('/');
                    if path.starts_with(root_str) {
                        path[root_str.len()..].trim_start_matches('/').to_string()
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

pub async fn load_cdr_data(state: &ConsoleState, record: &CallRecordModel) -> Option<CdrData> {
    let storage = resolve_cdr_storage(state);
    let callrecord: CallRecord = record.clone().into();
    let candidate = state.callrecord_formatter.format_file_name(&callrecord);
    let mut content: Option<String> = None;

    if let Some(ref storage_ref) = storage {
        let path_to_read = strip_storage_root(state, &candidate);

        match storage_ref.read_to_string(&path_to_read).await {
            Ok(value) => content = Some(value),
            Err(err) => {
                warn!(call_id = %record.call_id, path = %path_to_read, "failed to load CDR from storage: {}", err);
            }
        }
    }
    if let Some(raw) = content {
        match serde_json::from_str::<CallRecord>(&raw) {
            Ok(parsed) => {
                return Some(CdrData {
                    record: parsed,
                    raw_content: raw,
                    cdr_path: candidate,
                    storage: storage.clone(),
                });
            }
            Err(err) => {
                warn!(call_id = %record.call_id, path = %candidate, "failed to parse CDR file: {}", err);
            }
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

fn build_detail_payload(
    record: &CallRecordModel,
    related: &RelatedContext,
    state: &ConsoleState,
    cdr: Option<&CdrData>,
) -> Value {
    let inline_recording_url = select_recording_path(record, cdr)
        .map(|_| state.url_for(&format!("/call-records/{}/recording", record.id)));
    let record_payload =
        build_record_payload(record, related, state, inline_recording_url.as_deref());
    let participants = build_participants(record, related);

    let media_metrics = json!({
        "audio_codec": Value::Null,
        "rtp_packets": 0,
        "rtcp_observations": Value::Array(vec![]),
    });

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

    let metadata_download = if let Some(data) = cdr {
        Value::String(state.url_for(&format!(
            "/call-records/{}/metadata?path={}",
            record.id,
            encode(&data.cdr_path)
        )))
    } else {
        Value::Null
    };

    let sip_flow_download = state.url_for(&format!("/call-records/{}/sip-flow", record.id));

    let mut download_recording = derive_recording_download_url(state, record);
    if download_recording.is_none() {
        download_recording = inline_recording_url.clone();
    }

    json!({
        "back_url": state.url_for("/call-records"),
        "record": record_payload,
        //"sip_flow": sip_flow_download,
        "media_metrics": media_metrics,
        "notes": Value::Null,
        "participants": participants,
        "signaling": signaling,
        "rewrite": rewrite,
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

async fn build_summary(_db: &DatabaseConnection, _condition: Condition) -> Result<Value, DbErr> {
    Ok(json!({
        "total": 0,
        "answered": 0,
        "missed": 0,
        "failed": 0,
        "avg_duration": 0.0,
        "total_minutes": 0.0,
        "inbound": 0,
        "outbound": 0,
        "asr": 0.0,
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
            Arc::new(crate::callrecord::DefaultCallRecordFormatter::default()),
            db,
            ConsoleConfig {
                session_secret: "secret".into(),
                base_path: "/console".into(),
                allow_registration: false,
                secure_cookie: false,
                alpine_js: None,
                tailwind_js: None,
                chart_js: None,
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
    }
}
