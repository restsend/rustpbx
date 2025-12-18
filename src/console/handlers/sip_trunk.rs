use super::bad_request;
#[cfg(feature = "addon-wholesale")]
use crate::addons::wholesale::models::{
    tenant::Entity as TenantEntity,
    tenant_trunk::{
        ActiveModel as TenantTrunkActiveModel, Column as TenantTrunkColumn,
        Entity as TenantTrunkEntity,
    },
};
use crate::{
    console::handlers::forms::{self, ListQuery, SipTrunkForm},
    console::{ConsoleState, middleware::AuthRequired},
    models::sip_trunk::{
        ActiveModel as SipTrunkActiveModel, Column as SipTrunkColumn, Entity as SipTrunkEntity,
        SipTransport, SipTrunkDirection, SipTrunkStatus,
    },
};
use axum::{
    Json, Router,
    extract::{Form, Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::Order;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection, EntityTrait,
    Iterable, PaginatorTrait, QueryFilter, QueryOrder,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, Default, Deserialize)]
struct QuerySipTrunkFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    status: Option<SipTrunkStatus>,
    #[serde(default)]
    direction: Option<SipTrunkDirection>,
    #[serde(default)]
    transport: Option<SipTransport>,
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
    let (filters, tenants) = build_filters_payload(state.db()).await;
    state.render(
        "console/sip_trunk_detail.html",
        json!({
            "nav_active": "sip-trunk",
            "filters": filters,
            "tenants": tenants,
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
    let (filters, tenants) = build_filters_payload(db).await;

    let result = SipTrunkEntity::find_by_id(id).one(db).await;

    #[cfg(feature = "addon-wholesale")]
    let tenant_link = match TenantTrunkEntity::find()
        .filter(TenantTrunkColumn::SipTrunkId.eq(id))
        .all(db)
        .await
    {
        Ok(links) => {
            let link = links.into_iter().next();
            if let Some(ref l) = link {
                warn!(
                    "Found tenant link for trunk {}: tenant_id={}",
                    id, l.tenant_id
                );
            } else {
                warn!("No tenant link found for trunk {}", id);
            }
            link
        }
        Err(err) => {
            warn!("Failed to fetch tenant link for trunk {}: {}", id, err);
            None
        }
    };

    #[cfg(not(feature = "addon-wholesale"))]
    let tenant_link: Option<serde_json::Value> = None;

    match result {
        Ok(Some(model)) => {
            #[allow(unused_mut)]
            let mut model_json = serde_json::to_value(&model).unwrap_or(json!({}));

            #[cfg(feature = "addon-wholesale")]
            if let Some(obj) = model_json.as_object_mut() {
                if let Some(link) = tenant_link {
                    obj.insert("tenant_id".to_string(), json!(link.tenant_id));
                }
            }

            #[cfg(not(feature = "addon-wholesale"))]
            {
                let _ = tenant_link;
            }

            state.render(
                "console/sip_trunk_detail.html",
                json!({
                    "nav_active": "sip-trunk",
                    "model": model_json,
                    "filters": filters,
                    "tenants": tenants,
                    "mode": "edit",
                    "update_url": state.url_for(&format!("/sip-trunk/{id}")),
                }),
            )
        }
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
    let now = Utc::now();
    let mut active = SipTrunkActiveModel {
        ..Default::default()
    };

    if let Err(response) = apply_form_to_active_model(&mut active, &form, now, false) {
        return response;
    }

    match active.insert(db).await {
        Ok(model) => {
            if let Err(err) = handle_tenant_update(
                db,
                model.id,
                form.tenant_id,
                form.clear_tenant.unwrap_or(false),
            )
            .await
            {
                warn!(
                    "failed to update tenant link for trunk {}: {}",
                    model.id, err
                );
            }
            Json(json!({"status": "ok", "id": model.id})).into_response()
        }
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
    let now = Utc::now();
    if let Err(response) = apply_form_to_active_model(&mut active, &form, now, true) {
        return response;
    }

    match active.update(db).await {
        Ok(model) => {
            if let Err(err) = handle_tenant_update(
                db,
                model.id,
                form.tenant_id,
                form.clear_tenant.unwrap_or(false),
            )
            .await
            {
                warn!(
                    "failed to update tenant link for trunk {}: {}",
                    model.id, err
                );
            }
            Json(json!({"status": "ok"})).into_response()
        }
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
    AuthRequired(_): AuthRequired,
    Json(payload): Json<ListQuery<QuerySipTrunkFilters>>,
) -> Response {
    let db = state.db();
    let filters_payload;
    {
        let (payload, _) = build_filters_payload(db).await;
        filters_payload = payload;
    }

    let filters = payload.filters.clone().unwrap_or_default();
    let (_, per_page) = payload.normalize();

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

    if let Some(status) = filters.status {
        selector = selector.filter(SipTrunkColumn::Status.eq(status));
    }

    if let Some(direction) = filters.direction {
        selector = selector.filter(SipTrunkColumn::Direction.eq(direction));
    }

    if let Some(transport) = filters.transport {
        selector = selector.filter(SipTrunkColumn::SipTransport.eq(transport));
    }

    if filters.only_active.unwrap_or(false) {
        selector = selector.filter(SipTrunkColumn::IsActive.eq(true));
    }

    let sort_key = payload.sort.as_deref().unwrap_or("updated_at_desc");
    match sort_key {
        "updated_at_asc" => {
            selector = selector.order_by(SipTrunkColumn::UpdatedAt, Order::Asc);
        }
        "name_asc" => {
            selector = selector
                .order_by(SipTrunkColumn::DisplayName, Order::Asc)
                .order_by(SipTrunkColumn::Name, Order::Asc);
        }
        "name_desc" => {
            selector = selector
                .order_by(SipTrunkColumn::DisplayName, Order::Desc)
                .order_by(SipTrunkColumn::Name, Order::Desc);
        }
        "carrier_asc" => {
            selector = selector.order_by(SipTrunkColumn::Carrier, Order::Asc);
        }
        "carrier_desc" => {
            selector = selector.order_by(SipTrunkColumn::Carrier, Order::Desc);
        }
        "status_asc" => {
            selector = selector.order_by(SipTrunkColumn::Status, Order::Asc);
        }
        "status_desc" => {
            selector = selector.order_by(SipTrunkColumn::Status, Order::Desc);
        }
        _ => {
            selector = selector.order_by(SipTrunkColumn::UpdatedAt, Order::Desc);
        }
    }
    selector = selector.order_by(SipTrunkColumn::Id, Order::Desc);

    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, &payload).await {
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
        has_prev,
        has_next,
    } = pagination;

    let enriched_items: Vec<Value> = items
        .into_iter()
        .map(|model| serde_json::to_value(&model).unwrap_or_else(|_| json!({})))
        .collect();

    Json(json!({
        "page": current_page,
        "per_page": per_page,
        "total_items": total_items,
        "total_pages": total_pages,
        "has_prev": has_prev,
        "has_next": has_next,
        "items": enriched_items,
        "filters": filters_payload,
    }))
    .into_response()
}

async fn build_filters_payload(db: &DatabaseConnection) -> (Value, Vec<Value>) {
    let tenants = load_tenants(db).await;

    (
        json!({
            "statuses": SipTrunkStatus::iter()
                .map(|status| status.as_str())
                .collect::<Vec<_>>(),
            "directions": SipTrunkDirection::iter()
                .map(|direction| direction.as_str())
                .collect::<Vec<_>>(),
            "transports": SipTransport::iter()
                .map(|transport| transport.as_str())
                .collect::<Vec<_>>(),
        }),
        tenants,
    )
}

async fn load_tenants(db: &DatabaseConnection) -> Vec<Value> {
    #[cfg(feature = "addon-wholesale")]
    match TenantEntity::find()
        .order_by_asc(crate::addons::wholesale::models::tenant::Column::Name)
        .all(db)
        .await
    {
        Ok(list) => list
            .into_iter()
            .map(|t| serde_json::to_value(t).unwrap_or(json!({})))
            .collect(),
        Err(err) => {
            warn!("failed to load tenants: {}", err);
            vec![]
        }
    }

    #[cfg(not(feature = "addon-wholesale"))]
    {
        let _ = db;
        vec![]
    }
}

async fn handle_tenant_update(
    db: &DatabaseConnection,
    trunk_id: i64,
    tenant_id: Option<i64>,
    clear_tenant: bool,
) -> Result<(), sea_orm::DbErr> {
    #[cfg(feature = "addon-wholesale")]
    {
        if clear_tenant {
            TenantTrunkEntity::delete_many()
                .filter(TenantTrunkColumn::SipTrunkId.eq(trunk_id))
                .exec(db)
                .await?;
        } else if let Some(tid) = tenant_id {
            // Always clear existing links to ensure 1-to-1 relationship (Trunk -> Tenant)
            TenantTrunkEntity::delete_many()
                .filter(TenantTrunkColumn::SipTrunkId.eq(trunk_id))
                .exec(db)
                .await?;

            let active = TenantTrunkActiveModel {
                sip_trunk_id: Set(trunk_id),
                tenant_id: Set(tid),
                ..Default::default()
            };
            active.insert(db).await?;
        }
    }
    #[cfg(not(feature = "addon-wholesale"))]
    {
        let _ = db;
        let _ = trunk_id;
        let _ = tenant_id;
        let _ = clear_tenant;
    }
    Ok(())
}

fn apply_form_to_active_model(
    active: &mut SipTrunkActiveModel,
    form: &SipTrunkForm,
    now: DateTime<Utc>,
    is_update: bool,
) -> Result<(), Response> {
    let allowed_ips = parse_list_field(
        &form.allowed_ips,
        "allowed_ips",
        &["cidr", "ip", "host", "value"],
    )?;
    let did_numbers = parse_list_field(
        &form.did_numbers,
        "did_numbers",
        &["number", "did", "value"],
    )?;
    let billing_snapshot = parse_json_field(&form.billing_snapshot, "billing_snapshot")?;
    let analytics = parse_json_field(&form.analytics, "analytics")?;
    let tags = parse_json_field(&form.tags, "tags")?;
    let metadata = parse_json_field(&form.metadata, "metadata")?;

    if !is_update {
        let name = super::require_field(&form.name, "name")?;
        active.name = Set(name);
        active.status = Set(form.status.unwrap_or_default());
        active.direction = Set(form.direction.unwrap_or_default());
        active.sip_transport = Set(form.sip_transport.unwrap_or_default());
        active.is_active = Set(form.is_active.unwrap_or(true));
        active.created_at = Set(now);
    } else {
        if let Some(name) = super::normalize_optional_string(&form.name) {
            active.name = Set(name);
        }
        if let Some(status) = form.status {
            active.status = Set(status);
        }
        if let Some(direction) = form.direction {
            active.direction = Set(direction);
        }
        if let Some(transport) = form.sip_transport {
            active.sip_transport = Set(transport);
        }
        if let Some(is_active) = form.is_active {
            active.is_active = Set(is_active);
        }
    }

    if !is_update || form.display_name.is_some() {
        active.display_name = Set(super::normalize_optional_string(&form.display_name));
    }
    if !is_update || form.carrier.is_some() {
        active.carrier = Set(super::normalize_optional_string(&form.carrier));
    }
    if !is_update || form.description.is_some() {
        active.description = Set(super::normalize_optional_string(&form.description));
    }
    if !is_update || form.sip_server.is_some() {
        active.sip_server = Set(super::normalize_optional_string(&form.sip_server));
    }
    if !is_update || form.outbound_proxy.is_some() {
        active.outbound_proxy = Set(super::normalize_optional_string(&form.outbound_proxy));
    }
    if !is_update || form.auth_username.is_some() {
        active.auth_username = Set(super::normalize_optional_string(&form.auth_username));
    }
    if !is_update || form.auth_password.is_some() {
        active.auth_password = Set(super::normalize_optional_string(&form.auth_password));
    }
    if !is_update || form.default_route_label.is_some() {
        active.default_route_label =
            Set(super::normalize_optional_string(&form.default_route_label));
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
    if !is_update || form.incoming_from_user_prefix.is_some() {
        active.incoming_from_user_prefix = Set(super::normalize_optional_string(
            &form.incoming_from_user_prefix,
        ));
    }
    if !is_update || form.incoming_to_user_prefix.is_some() {
        active.incoming_to_user_prefix = Set(super::normalize_optional_string(
            &form.incoming_to_user_prefix,
        ));
    }
    if !is_update || form.metadata.is_some() {
        active.metadata = Set(metadata);
    }

    active.updated_at = Set(now);

    Ok(())
}

fn parse_list_field(
    value: &Option<String>,
    field: &str,
    preferred_keys: &[&str],
) -> Result<Option<Value>, Response> {
    let Some(raw) = value.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    if let Ok(json_value) = serde_json::from_str::<Value>(raw) {
        let normalized = normalize_list_json(json_value, field, preferred_keys)?;
        return Ok(
            normalized.map(|list| Value::Array(list.into_iter().map(Value::String).collect()))
        );
    }

    let entries: Vec<Value> = raw
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| Value::String(line.to_string()))
        .collect();

    if entries.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Array(entries)))
    }
}

fn normalize_list_json(
    value: Value,
    field: &str,
    preferred_keys: &[&str],
) -> Result<Option<Vec<String>>, Response> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => {
            let mut entries = Vec::new();
            for item in items {
                match extract_list_entry(item, preferred_keys) {
                    Ok(Some(entry)) => entries.push(entry),
                    Ok(None) => {}
                    Err(_) => {
                        return Err(bad_request(format!(
                            "{field} entries must resolve to plain text values"
                        )));
                    }
                }
            }
            if entries.is_empty() {
                Ok(None)
            } else {
                Ok(Some(entries))
            }
        }
        other => match extract_list_entry(other, preferred_keys) {
            Ok(Some(entry)) => Ok(Some(vec![entry])),
            Ok(None) => Ok(None),
            Err(_) => Err(bad_request(format!(
                "{field} entries must resolve to plain text values"
            ))),
        },
    }
}

fn extract_list_entry(value: Value, preferred_keys: &[&str]) -> Result<Option<String>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Value::Number(n) => Ok(Some(n.to_string())),
        Value::Bool(b) => Ok(Some(b.to_string())),
        Value::Object(mut map) => {
            for key in preferred_keys {
                if let Some(Value::String(s)) = map.remove(*key) {
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(trimmed.to_string()));
                }
            }
            for (_, candidate) in map.into_iter() {
                if let Value::String(s) = candidate {
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    return Ok(Some(trimmed.to_string()));
                }
            }
            Err(())
        }
        _ => Err(()),
    }
}

fn parse_json_field(value: &Option<String>, field: &str) -> Result<Option<Value>, Response> {
    let Some(raw) = value.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    serde_json::from_str(raw)
        .map(Some)
        .map_err(|err| bad_request(format!("{} must be valid JSON: {}", field, err)))
}
