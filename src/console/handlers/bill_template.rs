use crate::{
    console::handlers::forms::{self, BillTemplateForm, ListQuery},
    console::{ConsoleState, middleware::AuthRequired},
    models::bill_template::{
        ActiveModel as BillTemplateActiveModel, Column as BillTemplateColumn,
        Entity as BillTemplateEntity,
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
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, Default, Deserialize)]
struct QueryBillTemplateFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    billing_interval: Option<String>,
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/bill-templates",
            get(page_bill_templates)
                .put(create_bill_template)
                .post(query_bill_templates),
        )
        .route("/bill-templates/new", get(page_bill_template_create))
        .route(
            "/bill-templates/{id}",
            get(page_bill_template_detail)
                .patch(update_bill_template)
                .delete(delete_bill_template),
        )
}

async fn page_bill_templates(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let filters = build_filters_payload(state.db()).await;
    state.render(
        "console/bill_templates.html",
        json!({
            "nav_active": "bill-templates",
            "filters": filters,
            "create_url": state.url_for("/bill-templates/new"),
        }),
    )
}

async fn page_bill_template_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let filters = build_filters_payload(state.db()).await;
    state.render(
        "console/bill_template_detail.html",
        json!({
            "nav_active": "bill-templates",
            "filters": filters,
            "mode": "create",
            "create_url": state.url_for("/bill-templates"),
        }),
    )
}

async fn page_bill_template_detail(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let filters = build_filters_payload(db).await;

    let result = BillTemplateEntity::find_by_id(id).one(db).await;

    match result {
        Ok(Some(model)) => state.render(
            "console/bill_template_detail.html",
            json!({
                "nav_active": "bill-templates",
                "model": model,
                "filters": filters,
                "mode": "edit",
                "update_url": state.url_for(&format!("/bill-templates/{id}")),
            }),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"message": "Billing template not found"})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to load billing template {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to load billing template"})),
            )
                .into_response()
        }
    }
}

async fn create_bill_template(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<BillTemplateForm>,
) -> Response {
    let db = state.db();
    let now = Utc::now().naive_utc();
    let mut active = BillTemplateActiveModel {
        ..Default::default()
    };

    if let Err(response) = apply_form_to_active_model(&mut active, &form, now, false) {
        return response;
    }

    match active.insert(db).await {
        Ok(model) => Json(json!({"status": "ok", "id": model.id})).into_response(),
        Err(err) => {
            warn!("failed to create billing template: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to create billing template"})),
            )
                .into_response()
        }
    }
}

async fn update_bill_template(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<BillTemplateForm>,
) -> Response {
    let db = state.db();
    let model = match BillTemplateEntity::find_by_id(id).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Billing template not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load billing template {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update billing template"})),
            )
                .into_response();
        }
    };

    let mut active: BillTemplateActiveModel = model.into();
    let now = Utc::now().naive_utc();
    if let Err(response) = apply_form_to_active_model(&mut active, &form, now, true) {
        return response;
    }

    match active.update(db).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to update billing template {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to update billing template"})),
            )
                .into_response()
        }
    }
}

async fn delete_bill_template(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let model = match BillTemplateEntity::find_by_id(id).one(db).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Billing template not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load billing template {} for delete: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete billing template"})),
            )
                .into_response();
        }
    };

    let active: BillTemplateActiveModel = model.into();
    match active.delete(db).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to delete billing template {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to delete billing template"})),
            )
                .into_response()
        }
    }
}

async fn query_bill_templates(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<ListQuery<QueryBillTemplateFilters>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let filters_payload = build_filters_payload(db).await;

    let filters = query.filters.clone().unwrap_or_default();
    let (_, per_page) = query.normalize();

    let mut selector = BillTemplateEntity::find();

    if let Some(ref q_raw) = filters.q {
        let trimmed = q_raw.trim();
        if !trimmed.is_empty() {
            let mut condition = Condition::any();
            condition = condition.add(BillTemplateColumn::Name.contains(trimmed));
            condition = condition.add(BillTemplateColumn::DisplayName.contains(trimmed));
            selector = selector.filter(condition);
        }
    }

    if let Some(ref currency_raw) = filters.currency {
        let currency = currency_raw.trim().to_uppercase();
        if !currency.is_empty() {
            selector = selector.filter(BillTemplateColumn::Currency.eq(currency));
        }
    }

    if let Some(ref interval_raw) = filters.billing_interval {
        let interval = interval_raw.trim().to_lowercase();
        if !interval.is_empty() {
            selector = selector.filter(BillTemplateColumn::BillingInterval.eq(interval));
        }
    }

    selector = selector.order_by_desc(BillTemplateColumn::UpdatedAt);

    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, query).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to paginate billing templates: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to query billing templates"})),
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

    let serialized_items: Vec<Value> = items
        .into_iter()
        .map(|model| serde_json::to_value(&model).unwrap_or_else(|_| json!({})))
        .collect();

    Json(json!({
        "pagination": {
            "items": serialized_items,
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

async fn build_filters_payload(db: &DatabaseConnection) -> Value {
    match BillTemplateEntity::find().all(db).await {
        Ok(templates) => {
            let mut currencies: Vec<String> = templates
                .iter()
                .map(|tpl| tpl.currency.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect();
            currencies.sort();
            currencies.dedup();

            let mut intervals: Vec<String> = templates
                .iter()
                .map(|tpl| tpl.billing_interval.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            intervals.sort();
            intervals.dedup();

            json!({
                "currencies": currencies,
                "billing_intervals": intervals,
            })
        }
        Err(err) => {
            warn!("failed to load billing templates for filters: {}", err);
            json!({
                "currencies": Vec::<String>::new(),
                "billing_intervals": Vec::<String>::new(),
            })
        }
    }
}

fn apply_form_to_active_model(
    active: &mut BillTemplateActiveModel,
    form: &BillTemplateForm,
    now: NaiveDateTime,
    is_update: bool,
) -> Result<(), Response> {
    let metadata = parse_json_field(&form.metadata, "metadata")?;

    if !is_update {
        let name = require_field(&form.name, "name")?;
        active.name = Set(name);
        active.currency = Set(normalize_currency(form.currency.clone(), "USD")?);
        active.billing_interval = Set(normalize_interval(
            form.billing_interval.clone(),
            "monthly",
        )?);
        active.included_minutes = Set(form.included_minutes.unwrap_or(0));
        active.included_messages = Set(form.included_messages.unwrap_or(0));
        active.overage_rate_per_minute = Set(form.overage_rate_per_minute.unwrap_or(0.0));
        active.setup_fee = Set(form.setup_fee.unwrap_or(0.0));
        active.tax_percent = Set(form.tax_percent.unwrap_or(0.0));
        active.created_at = Set(now);
    } else {
        if let Some(name) = normalize_optional_string(&form.name) {
            active.name = Set(name);
        }
        if let Some(currency) = normalize_currency_optional(&form.currency)? {
            active.currency = Set(currency);
        }
        if let Some(interval) = normalize_interval_optional(&form.billing_interval)? {
            active.billing_interval = Set(interval);
        }
        if form.included_minutes.is_some() {
            active.included_minutes = Set(form.included_minutes.unwrap_or(0));
        }
        if form.included_messages.is_some() {
            active.included_messages = Set(form.included_messages.unwrap_or(0));
        }
        if form.overage_rate_per_minute.is_some() {
            active.overage_rate_per_minute = Set(form.overage_rate_per_minute.unwrap_or(0.0));
        }
        if form.setup_fee.is_some() {
            active.setup_fee = Set(form.setup_fee.unwrap_or(0.0));
        }
        if form.tax_percent.is_some() {
            active.tax_percent = Set(form.tax_percent.unwrap_or(0.0));
        }
    }

    if !is_update || form.display_name.is_some() {
        active.display_name = Set(normalize_optional_string(&form.display_name));
    }
    if !is_update || form.description.is_some() {
        active.description = Set(normalize_optional_string(&form.description));
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

fn normalize_currency(value: Option<String>, default: &str) -> Result<String, Response> {
    if let Some(raw) = value
        .map(|v| v.trim().to_uppercase())
        .filter(|v| !v.is_empty())
    {
        Ok(raw)
    } else if default.is_empty() {
        Err(bad_request("currency is required"))
    } else {
        Ok(default.to_string())
    }
}

fn normalize_currency_optional(value: &Option<String>) -> Result<Option<String>, Response> {
    if let Some(raw) = value
        .as_ref()
        .map(|v| v.trim().to_uppercase())
        .filter(|v| !v.is_empty())
    {
        Ok(Some(raw))
    } else if value.is_some() {
        Err(bad_request("currency cannot be empty"))
    } else {
        Ok(None)
    }
}

fn normalize_interval(value: Option<String>, default: &str) -> Result<String, Response> {
    if let Some(raw) = value
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
    {
        Ok(raw)
    } else if default.is_empty() {
        Err(bad_request("billing_interval is required"))
    } else {
        Ok(default.to_string())
    }
}

fn normalize_interval_optional(value: &Option<String>) -> Result<Option<String>, Response> {
    if let Some(raw) = value
        .as_ref()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
    {
        Ok(Some(raw))
    } else if value.is_some() {
        Err(bad_request("billing_interval cannot be empty"))
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
