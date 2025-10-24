use crate::console::{ConsoleState, handlers::forms, middleware::AuthRequired};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::warn;

use crate::models::{
    call_record::{
        Column as CallRecordColumn, Entity as CallRecordEntity, Model as CallRecordModel,
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
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/call-records",
            get(page_call_records).post(query_call_records),
        )
        .route("/call-records/{id}", get(page_call_record_detail))
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

    let selector = CallRecordEntity::find()
        .filter(condition.clone())
        .order_by_desc(CallRecordColumn::StartedAt);

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

    let items: Vec<Value> = pagination
        .items
        .iter()
        .map(|record| build_record_payload(record, &related, &state))
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

    let payload = build_detail_payload(&model, &related, &state);

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

fn build_record_payload(
    record: &CallRecordModel,
    related: &RelatedContext,
    state: &ConsoleState,
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

    let recording = record.recording_url.as_ref().map(|url| {
        json!({
            "url": url,
            "duration_secs": record.recording_duration_secs,
        })
    });

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
    })
}

fn build_detail_payload(
    record: &CallRecordModel,
    related: &RelatedContext,
    state: &ConsoleState,
) -> Value {
    let record_payload = build_record_payload(record, related, state);
    let participants = build_participants(record, related);
    let transcript_payload = json!({
        "available": record.has_transcript,
        "language": record.transcript_language,
        "status": record.transcript_status,
        "generated_at": record.updated_at.to_rfc3339(),
        "segments": Value::Array(vec![]),
        "text": "",
    });

    let media_metrics = json!({
        "audio_codec": json_lookup_str(&record.metadata, "audio_codec"),
        "rtp_packets": json_lookup_number(&record.analytics, "rtp_packets"),
        "avg_jitter_ms": record.quality_jitter_ms,
        "packet_loss_percent": record.quality_packet_loss_percent,
        "mos": record.quality_mos,
        "rtcp_observations": Value::Array(vec![]),
    });

    let signaling = record.signaling.clone().unwrap_or(Value::Null);

    json!({
        "back_url": state.url_for("/call-records"),
        "record": record_payload,
        "sip_flow": Value::Array(vec![]),
        "media_metrics": media_metrics,
        "transcript": transcript_payload,
        "transcript_preview": Value::Null,
        "notes": Value::Null,
        "participants": participants,
        "signaling": signaling,
        "actions": json!({
            "download_recording": record.recording_url,
            "download_metadata": Value::Null,
            "download_sip_flow": Value::Null,
        }),
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
        .filter(condition)
        .into_tuple::<Option<i64>>()
        .one(db)
        .await?
        .flatten()
        .unwrap_or(0);

    Ok(json!({
        "total": total,
        "answered": answered,
        "missed": missed,
        "failed": failed,
        "transcribed": transcribed,
        "avg_duration": (avg_duration * 10.0).round() / 10.0,
        "total_minutes": (total_minutes * 10.0).round() / 10.0,
        "unique_dids": unique_dids,
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
        let payload = build_record_payload(&record, &related, &state);
        assert_eq!(payload["id"], 1);
    }
}
