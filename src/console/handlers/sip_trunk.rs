use crate::{
    console::handlers::forms::{self, ListQuery, SipTrunkForm},
    console::{ConsoleState, middleware::AuthRequired},
    models::{
        bill_template::{
            Column as BillTemplateColumn, Entity as BillTemplateEntity, Model as BillTemplateModel,
        },
        sip_trunk::{
            ActiveModel as SipTrunkActiveModel, Column as SipTrunkColumn, Entity as SipTrunkEntity,
        },
    },
};
use axum::{
    Json, Router,
    extract::{Form, Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{NaiveDateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tracing::warn;

const ALLOWED_DIRECTIONS: &[&str] = &["inbound", "outbound", "bidirectional"];
const ALLOWED_TRANSPORTS: &[&str] = &["udp", "tcp", "tls"];

#[derive(Debug, Clone, Default, Deserialize)]
struct QuerySipTrunkFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    billing_template_ids: Option<Vec<i64>>,
    #[serde(default)]
    only_active: Option<bool>,
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/sip-trunk",
            get(page_sip_trunks)
                .put(create_sip_trunk)
                .post(query_sip_trunks),
        )
        .route("/sip-trunk/new", get(page_sip_trunk_create))
        .route(
            "/sip-trunk/{id}",
            get(page_sip_trunk_detail)
                .patch(update_sip_trunk)
                .delete(delete_sip_trunk),
        )
}

async fn page_sip_trunks(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let (filters, _) = build_filters_payload(state.db()).await;
    state.render(
        "console/sip_trunk.html",
        json!({
            "nav_active": "sip-trunk",
            "filters": filters,
            "create_url": state.url_for("/sip-trunk/new"),
        }),
    )
}

async fn page_sip_trunk_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let (filters, templates) = build_filters_payload(state.db()).await;
    state.render(
        "console/sip_trunk_detail.html",
        json!({
            "nav_active": "sip-trunk",
            "filters": filters,
            "templates": templates,
            "mode": "create",
            "create_url": state.url_for("/sip-trunk"),
        }),
    )
}

async fn page_sip_trunk_detail(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let (filters, templates) = build_filters_payload(db).await;

    let result = SipTrunkEntity::find_by_id(id)
        .find_also_related(BillTemplateEntity)
        .one(db)
        .await;

    match result {
        Ok(Some((model, template))) => state.render(
            "console/sip_trunk_detail.html",
            json!({
                "nav_active": "sip-trunk",
                "model": model,
                "bill_template": template,
                "filters": filters,
                "templates": templates,
                "mode": "edit",
                "update_url": state.url_for(&format!("/sip-trunk/{id}")),
            }),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"message": "SIP trunk not found"})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to load sip trunk {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load SIP trunk"})),
            )
                .into_response()
        }
    }
}

async fn create_sip_trunk(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<SipTrunkForm>,
) -> Response {
    let db = state.db();
    let now = Utc::now().naive_utc();
    let mut active = SipTrunkActiveModel {
        ..Default::default()
    };

    if let Err(response) = apply_form_to_active_model(&mut active, &form, now, false) {
        return response;
    }

    match active.insert(db).await {
        Ok(model) => Json(json!({"status": "ok", "id": model.id})).into_response(),
        Err(err) => {
            warn!("failed to create sip trunk: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create SIP trunk"})),
            )
                .into_response()
        }
    }
}

async fn update_sip_trunk(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<SipTrunkForm>,
) -> Response {
    let db = state.db();
    let model = match SipTrunkEntity::find_by_id(id).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "SIP trunk not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load sip trunk {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update SIP trunk"})),
            )
                .into_response();
        }
    };

    let mut active: SipTrunkActiveModel = model.into();
    let now = Utc::now().naive_utc();
    if let Err(response) = apply_form_to_active_model(&mut active, &form, now, true) {
        return response;
    }

    match active.update(db).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to update sip trunk {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update SIP trunk"})),
            )
                .into_response()
        }
    }
}

async fn delete_sip_trunk(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match SipTrunkEntity::find_by_id(id).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "SIP trunk not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load sip trunk {} for delete: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete SIP trunk"})),
            )
                .into_response();
        }
    };

    let active: SipTrunkActiveModel = model.into();
    match active.delete(db).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to delete sip trunk {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete SIP trunk"})),
            )
                .into_response()
        }
    }
}

async fn query_sip_trunks(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<ListQuery<QuerySipTrunkFilters>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let filters_payload;
    let templates;
    {
        let (payload, list) = build_filters_payload(db).await;
        filters_payload = payload;
        templates = list;
    }
    let template_lookup: HashMap<i64, BillTemplateModel> =
        templates.into_iter().map(|tpl| (tpl.id, tpl)).collect();

    let filters = query.filters.clone().unwrap_or_default();
    let (_, per_page) = query.normalize();

    let mut selector = SipTrunkEntity::find();

    if let Some(ref q_raw) = filters.q {
        let trimmed = q_raw.trim();
        if !trimmed.is_empty() {
            let mut condition = Condition::any();
            condition = condition.add(SipTrunkColumn::Name.contains(trimmed));
            condition = condition.add(SipTrunkColumn::DisplayName.contains(trimmed));
            condition = condition.add(SipTrunkColumn::Carrier.contains(trimmed));
            condition = condition.add(SipTrunkColumn::SipServer.contains(trimmed));
            selector = selector.filter(condition);
        }
    }

    if let Some(ref status_raw) = filters.status {
        let status = status_raw.trim().to_lowercase();
        if !status.is_empty() {
            selector = selector.filter(SipTrunkColumn::Status.eq(status));
        }
    }

    if let Some(ref direction_raw) = filters.direction {
        let direction = direction_raw.trim().to_lowercase();
        if !direction.is_empty() {
            selector = selector.filter(SipTrunkColumn::Direction.eq(direction));
        }
    }

    if let Some(template_ids) = filters
        .billing_template_ids
        .as_ref()
        .filter(|ids| !ids.is_empty())
    {
        selector = selector.filter(SipTrunkColumn::BillingTemplateId.is_in(template_ids.clone()));
    }

    if filters.only_active.unwrap_or(false) {
        selector = selector.filter(SipTrunkColumn::IsActive.eq(true));
    }

    selector = selector.order_by_desc(SipTrunkColumn::UpdatedAt);

    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, query).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to paginate sip trunks: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to query SIP trunks"})),
            )
                .into_response();
        }
    };

    let forms::Pagination {
        items,
        current_page,
        per_page,
        total_items,
        total_pages,
        showing_from,
        showing_to,
        has_prev,
        has_next,
        prev_page,
        next_page,
    } = pagination;

    let enriched_items: Vec<Value> = items
        .into_iter()
        .map(|model| {
            let mut value = serde_json::to_value(&model).unwrap_or_else(|_| json!({}));
            if let Some(obj) = value.as_object_mut() {
                if let Some(template_id) = model.billing_template_id {
                    if let Some(template) = template_lookup.get(&template_id) {
                        obj.insert(
                            "billing_template".to_string(),
                            json!({
                                "id": template.id,
                                "name": template.name,
                                "display_name": template
                                    .display_name
                                    .clone()
                                    .unwrap_or_else(|| template.name.clone()),
                            }),
                        );
                    }
                }
            }
            value
        })
        .collect();

    Json(json!({
        "pagination": {
            "items": enriched_items,
            "current_page": current_page,
            "per_page": per_page,
            "total_items": total_items,
            "total_pages": total_pages,
            "showing_from": showing_from,
            "showing_to": showing_to,
            "has_prev": has_prev,
            "has_next": has_next,
            "prev_page": prev_page,
            "next_page": next_page,
        },
        "filters": filters_payload,
    }))
    .into_response()
}

async fn build_filters_payload(db: &DatabaseConnection) -> (Value, Vec<BillTemplateModel>) {
    let templates = load_bill_templates(db).await;
    let template_values: Vec<Value> = templates
        .iter()
        .map(|tpl| {
            json!({
                "id": tpl.id,
                "name": tpl.name,
                "display_name": tpl
                    .display_name
                    .clone()
                    .unwrap_or_else(|| tpl.name.clone()),
            })
        })
        .collect();

    (
        json!({
            "bill_templates": template_values,
            "statuses": ["healthy", "warning", "standby", "offline"],
            "directions": ["inbound", "outbound", "bidirectional"],
        }),
        templates,
    )
}

async fn load_bill_templates(db: &DatabaseConnection) -> Vec<BillTemplateModel> {
    match BillTemplateEntity::find()
        .order_by_asc(BillTemplateColumn::DisplayName)
        .order_by_asc(BillTemplateColumn::Name)
        .all(db)
        .await
    {
        Ok(list) => list,
        Err(err) => {
            warn!("failed to load bill templates: {}", err);
            vec![]
        }
    }
}

fn apply_form_to_active_model(
    active: &mut SipTrunkActiveModel,
    form: &SipTrunkForm,
    now: NaiveDateTime,
    is_update: bool,
) -> Result<(), Response> {
    let allowed_ips = parse_json_field(&form.allowed_ips, "allowed_ips")?;
    let did_numbers = parse_json_field(&form.did_numbers, "did_numbers")?;
    let billing_snapshot = parse_json_field(&form.billing_snapshot, "billing_snapshot")?;
    let analytics = parse_json_field(&form.analytics, "analytics")?;
    let tags = parse_json_field(&form.tags, "tags")?;
    let metadata = parse_json_field(&form.metadata, "metadata")?;

    if !is_update {
        let name = require_field(&form.name, "name")?;
        active.name = Set(name);
        active.status = Set(normalize_status(form.status.clone(), "healthy"));
        active.direction = Set(normalize_direction(form.direction.clone())?);
        active.sip_transport = Set(normalize_transport(form.sip_transport.clone())?);
        active.is_active = Set(form.is_active.unwrap_or(true));
        active.created_at = Set(now);
    } else {
        if let Some(name) = normalize_optional_string(&form.name) {
            active.name = Set(name);
        }
        if let Some(status) = normalize_status_optional(&form.status) {
            active.status = Set(status);
        }
        if let Some(direction) = maybe_direction_update(&form.direction)? {
            active.direction = Set(direction);
        }
        if let Some(transport) = maybe_transport_update(&form.sip_transport)? {
            active.sip_transport = Set(transport);
        }
        if form.is_active.is_some() {
            active.is_active = Set(form.is_active.unwrap_or(true));
        }
    }

    if !is_update || form.display_name.is_some() {
        active.display_name = Set(normalize_optional_string(&form.display_name));
    }
    if !is_update || form.carrier.is_some() {
        active.carrier = Set(normalize_optional_string(&form.carrier));
    }
    if !is_update || form.description.is_some() {
        active.description = Set(normalize_optional_string(&form.description));
    }
    if !is_update || form.sip_server.is_some() {
        active.sip_server = Set(normalize_optional_string(&form.sip_server));
    }
    if !is_update || form.outbound_proxy.is_some() {
        active.outbound_proxy = Set(normalize_optional_string(&form.outbound_proxy));
    }
    if !is_update || form.auth_username.is_some() {
        active.auth_username = Set(normalize_optional_string(&form.auth_username));
    }
    if !is_update || form.auth_password.is_some() {
        active.auth_password = Set(normalize_optional_string(&form.auth_password));
    }
    if !is_update || form.default_route_label.is_some() {
        active.default_route_label = Set(normalize_optional_string(&form.default_route_label));
    }

    let clear_billing_template = form.clear_billing_template.unwrap_or(false);

    if clear_billing_template {
        active.billing_template_id = Set(None);
    } else if let Some(template_id) = form.billing_template_id {
        active.billing_template_id = Set(Some(template_id));
    } else if !is_update {
        active.billing_template_id = Set(None);
    }

    if !is_update || form.max_cps.is_some() {
        active.max_cps = Set(form.max_cps);
    }
    if !is_update || form.max_concurrent.is_some() {
        active.max_concurrent = Set(form.max_concurrent);
    }
    if !is_update || form.max_call_duration.is_some() {
        active.max_call_duration = Set(form.max_call_duration);
    }
    if !is_update || form.utilisation_percent.is_some() {
        active.utilisation_percent = Set(form.utilisation_percent);
    }
    if !is_update || form.warning_threshold_percent.is_some() {
        active.warning_threshold_percent = Set(form.warning_threshold_percent);
    }

    if !is_update || form.allowed_ips.is_some() {
        active.allowed_ips = Set(allowed_ips);
    }
    if !is_update || form.did_numbers.is_some() {
        active.did_numbers = Set(did_numbers);
    }
    if !is_update || form.billing_snapshot.is_some() {
        active.billing_snapshot = Set(billing_snapshot);
    }
    if !is_update || form.analytics.is_some() {
        active.analytics = Set(analytics);
    }
    if !is_update || form.tags.is_some() {
        active.tags = Set(tags);
    }
    if !is_update || form.metadata.is_some() {
        active.metadata = Set(metadata);
    }

    active.updated_at = Set(now);

    Ok(())
}

fn require_field(value: &Option<String>, field: &str) -> Result<String, Response> {
    normalize_optional_string(value).ok_or_else(|| bad_request(format!("{} is required", field)))
}

fn normalize_optional_string(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

fn parse_json_field(value: &Option<String>, field: &str) -> Result<Option<Value>, Response> {
    let Some(raw) = value.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    serde_json::from_str(raw)
        .map(Some)
        .map_err(|err| bad_request(format!("{} must be valid JSON: {}", field, err)))
}

fn normalize_status(value: Option<String>, default: &str) -> String {
    value
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn normalize_status_optional(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
}

fn normalize_direction(value: Option<String>) -> Result<String, Response> {
    let direction = value
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "bidirectional".to_string());

    if ALLOWED_DIRECTIONS.contains(&direction.as_str()) {
        Ok(direction)
    } else {
        Err(bad_request(format!(
            "direction must be one of {}",
            ALLOWED_DIRECTIONS.join(", ")
        )))
    }
}

fn maybe_direction_update(value: &Option<String>) -> Result<Option<String>, Response> {
    if let Some(raw) = value.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        let direction = raw.to_lowercase();
        if ALLOWED_DIRECTIONS.contains(&direction.as_str()) {
            Ok(Some(direction))
        } else {
            Err(bad_request(format!(
                "direction must be one of {}",
                ALLOWED_DIRECTIONS.join(", ")
            )))
        }
    } else if value.is_some() {
        Err(bad_request("direction cannot be empty"))
    } else {
        Ok(None)
    }
}

fn normalize_transport(value: Option<String>) -> Result<String, Response> {
    let transport = value
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "udp".to_string());

    if ALLOWED_TRANSPORTS.contains(&transport.as_str()) {
        Ok(transport)
    } else {
        Err(bad_request(format!(
            "sip_transport must be one of {}",
            ALLOWED_TRANSPORTS.join(", ")
        )))
    }
}

fn maybe_transport_update(value: &Option<String>) -> Result<Option<String>, Response> {
    if let Some(raw) = value.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        let transport = raw.to_lowercase();
        if ALLOWED_TRANSPORTS.contains(&transport.as_str()) {
            Ok(Some(transport))
        } else {
            Err(bad_request(format!(
                "sip_transport must be one of {}",
                ALLOWED_TRANSPORTS.join(", ")
            )))
        }
    } else if value.is_some() {
        Err(bad_request("sip_transport cannot be empty"))
    } else {
        Ok(None)
    }
}

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"message": message.into()})),
    )
        .into_response()
}
