use crate::callrecord::{
    CallRecord,
    sipflow::{SipFlowDirection, SipMessageItem},
};
use crate::config::CallRecordConfig;
use crate::console::{ConsoleState, handlers::forms, middleware::AuthRequired};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sea_orm::sea_query::{Expr, Order};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::warn;

const BILLING_STATUS_CHARGED: &str = "charged";
const BILLING_STATUS_INCLUDED: &str = "included";
const BILLING_STATUS_ZERO_DURATION: &str = "zero-duration";
const BILLING_STATUS_UNRATED: &str = "unrated";

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
    #[serde(default)]
    billing_status: Option<String>,
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/call-records",
            get(page_call_records).post(query_call_records),
        )
        .route(
            "/call-records/{id}",
            get(page_call_record_detail).delete(delete_call_record),
        )
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

    let cdr_data = load_cdr_data(&state, &model).await;

    let mut sip_flow_items: Vec<LeggedSipMessage> = if let Some(ref cdr) = cdr_data {
        load_sip_flow_from_files(&cdr.sip_flow_paths).await
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

    let payload = build_detail_payload(&model, &related, &state, sip_flow_items, cdr_data.as_ref());

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

#[derive(Debug, Clone)]
struct SipFlowFileRef {
    leg: Option<String>,
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
    sip_flow_paths: Vec<SipFlowFileRef>,
    cdr_path: String,
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

async fn load_cdr_data(state: &ConsoleState, record: &CallRecordModel) -> Option<CdrData> {
    let root = state.app_state().and_then(|app| {
        let config = app.config.clone();
        config.callrecord.as_ref().and_then(|cfg| match cfg {
            CallRecordConfig::Local { root } => Some(root.clone()),
            _ => None,
        })
    });

    let metadata_path = extract_cdr_path_from_metadata(&record.metadata);
    let mut candidates: Vec<String> = Vec::new();

    if let Some(path) = metadata_path {
        if Path::new(&path).is_absolute() {
            candidates.push(path);
        } else if let Some(ref root_path) = root {
            candidates.push(join_root_path(root_path, &path));
        } else {
            candidates.push(path);
        }
    }

    if candidates.is_empty() {
        if let Some(ref root_path) = root {
            let fallback = default_cdr_file_name_for_model(record);
            candidates.push(join_root_path(root_path, &fallback));
        }
    }

    for candidate in candidates {
        match tokio::fs::read_to_string(&candidate).await {
            Ok(content) => match serde_json::from_str::<CallRecord>(&content) {
                Ok(parsed) => {
                    let base_dir = Path::new(&candidate).parent().map(|p| p.to_path_buf());
                    let sip_flow_paths = extract_sip_flow_paths_from_cdr(
                        &parsed,
                        root.as_deref(),
                        base_dir.as_deref(),
                    );
                    return Some(CdrData {
                        record: parsed,
                        sip_flow_paths,
                        cdr_path: candidate,
                    });
                }
                Err(err) => {
                    warn!(call_id = %record.call_id, path = %candidate, "failed to parse CDR file: {}", err);
                }
            },
            Err(err) => {
                warn!(call_id = %record.call_id, path = %candidate, "failed to read CDR file: {}", err);
            }
        }
    }

    None
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

fn join_root_path(root: &str, relative: &str) -> String {
    let trimmed = relative.trim_start_matches(|c| c == '/' || c == '\\');
    PathBuf::from(root)
        .join(trimmed)
        .to_string_lossy()
        .into_owned()
}

fn extract_sip_flow_paths_from_cdr(
    record: &CallRecord,
    root: Option<&str>,
    base_dir: Option<&Path>,
) -> Vec<SipFlowFileRef> {
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
                        let trimmed = path.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if Path::new(trimmed).is_absolute() {
                            paths.push(SipFlowFileRef {
                                leg: leg.clone(),
                                path: trimmed.to_string(),
                            });
                            continue;
                        }

                        let relative = trimmed.trim_start_matches(|c| c == '/' || c == '\\');

                        if let Some(root_path) = root {
                            let candidate = join_root_path(root_path, relative);
                            if Path::new(&candidate).exists() {
                                paths.push(SipFlowFileRef {
                                    leg: leg.clone(),
                                    path: candidate,
                                });
                                continue;
                            }
                        }

                        if let Some(dir) = base_dir {
                            let candidate_path = dir.join(relative);
                            if candidate_path.exists() {
                                paths.push(SipFlowFileRef {
                                    leg: leg.clone(),
                                    path: candidate_path.to_string_lossy().into_owned(),
                                });
                                continue;
                            }
                        }

                        if let Some(root_path) = root {
                            paths.push(SipFlowFileRef {
                                leg: leg.clone(),
                                path: join_root_path(root_path, relative),
                            });
                        } else if let Some(dir) = base_dir {
                            paths.push(SipFlowFileRef {
                                leg: leg.clone(),
                                path: dir.join(relative).to_string_lossy().into_owned(),
                            });
                        } else {
                            paths.push(SipFlowFileRef {
                                leg: leg.clone(),
                                path: trimmed.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
    paths
}

async fn load_sip_flow_from_files(paths: &[SipFlowFileRef]) -> Vec<LeggedSipMessage> {
    let mut all_items = Vec::new();
    for entry in paths {
        let mut items = read_sip_flow_file(&entry.path).await;
        let leg = entry.leg.clone();
        for item in items.drain(..) {
            all_items.push(LeggedSipMessage {
                leg: leg.clone(),
                leg_role: None,
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
) -> Value {
    let record_payload = build_record_payload(record, related, state);
    let participants = build_participants(record, related);
    let billing_summary = record_payload
        .get("billing")
        .cloned()
        .unwrap_or_else(|| build_billing_payload(record));
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
        let metadata = Value::String(data.cdr_path.clone());
        let flow = data
            .sip_flow_paths
            .first()
            .map(|entry| Value::String(entry.path.clone()))
            .unwrap_or(Value::Null);
        (metadata, flow)
    } else {
        (Value::Null, Value::Null)
    };

    json!({
        "back_url": state.url_for("/call-records"),
        "record": record_payload,
        "sip_flow": sip_flow_value,
        "media_metrics": media_metrics,
        "transcript": transcript_payload,
        "transcript_preview": Value::Null,
        "notes": Value::Null,
        "participants": participants,
        "signaling": signaling,
        "rewrite": rewrite,
        "billing": json!({
            "summary": billing_summary,
            "snapshot": record.billing_snapshot.clone().unwrap_or(Value::Null),
            "result": record.billing_result.clone().unwrap_or(Value::Null),
        }),
        "actions": json!({
            "download_recording": record.recording_url,
            "download_metadata": metadata_download,
            "download_sip_flow": sip_flow_download,
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

    load_sip_flow_from_files(&paths).await
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
                    let trimmed = path.trim();
                    if !trimmed.is_empty() {
                        paths.push(SipFlowFileRef {
                            leg: leg.clone(),
                            path: trimmed.to_string(),
                        });
                    }
                }
            }
        }
    }
    paths
}

async fn read_sip_flow_file(path: &str) -> Vec<SipMessageItem> {
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
        let payload = build_record_payload(&record, &related, &state);
        assert_eq!(payload["id"], 1);
        assert!(payload.get("billing").is_some());
    }
}
