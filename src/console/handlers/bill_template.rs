use crate::{
    console::handlers::{
        bad_request,
        forms::{self, BillTemplatePayload, ListQuery},
    },
    console::{ConsoleState, middleware::AuthRequired},
    models::bill_template::{
        ActiveModel as BillTemplateActiveModel, BillingInterval, Column as BillTemplateColumn,
        Entity as BillTemplateEntity,
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
    ActiveModelTrait, ActiveValue, ActiveValue::Set, ColumnTrait, Condition, DatabaseConnection,
    EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
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
    billing_interval: Option<BillingInterval>,
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
    Form(form): Form<BillTemplatePayload>,
) -> Response {
    let db = state.db();
    let now = Utc::now();
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
    Form(form): Form<BillTemplatePayload>,
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
    let now = Utc::now();
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
    AuthRequired(_): AuthRequired,
    Json(payload): Json<ListQuery<QueryBillTemplateFilters>>,
) -> Response {
    let db = state.db();
    let filters_payload = build_filters_payload(db).await;

    let filters = payload.filters.clone().unwrap_or_default();
    let (_, per_page) = payload.normalize();

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

    if let Some(interval) = filters.billing_interval {
        selector = selector.filter(BillTemplateColumn::BillingInterval.eq(interval));
    }

    let sort_key = payload.sort.as_deref().unwrap_or("updated_at_desc");
    match sort_key {
        "updated_at_asc" => {
            selector = selector.order_by(BillTemplateColumn::UpdatedAt, Order::Asc);
        }
        "name_asc" => {
            selector = selector
                .order_by(BillTemplateColumn::DisplayName, Order::Asc)
                .order_by(BillTemplateColumn::Name, Order::Asc);
        }
        "name_desc" => {
            selector = selector
                .order_by(BillTemplateColumn::DisplayName, Order::Desc)
                .order_by(BillTemplateColumn::Name, Order::Desc);
        }
        "currency_asc" => {
            selector = selector.order_by(BillTemplateColumn::Currency, Order::Asc);
        }
        "currency_desc" => {
            selector = selector.order_by(BillTemplateColumn::Currency, Order::Desc);
        }
        "interval_asc" => {
            selector = selector.order_by(BillTemplateColumn::BillingInterval, Order::Asc);
        }
        "interval_desc" => {
            selector = selector.order_by(BillTemplateColumn::BillingInterval, Order::Desc);
        }
        _ => {
            selector = selector.order_by(BillTemplateColumn::UpdatedAt, Order::Desc);
        }
    }
    selector = selector.order_by(BillTemplateColumn::Id, Order::Desc);

    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, &payload).await {
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
        has_prev,
        has_next,
    } = pagination;

    let serialized_items: Vec<Value> = items
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
        "items": serialized_items,
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

            let mut intervals: Vec<_> = templates
                .iter()
                .filter_map(|tpl| Some(tpl.billing_interval.clone()))
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

fn normalize_increment_value(raw: i32, field: &str) -> Result<i32, Response> {
    if raw <= 0 {
        return Err(bad_request(format!("{field} must be greater than zero")));
    }
    if raw > 86_400 {
        return Err(bad_request(format!(
            "{field} must be less than or equal to 86400 seconds"
        )));
    }
    Ok(raw)
}

fn validate_billing_pair(initial: i32, billing: i32) -> Result<(), Response> {
    if billing > initial {
        return Err(bad_request(
            "billing_increment_secs cannot exceed initial_increment_secs",
        ));
    }
    Ok(())
}

fn active_value_i32(value: &ActiveValue<i32>, fallback: i32) -> i32 {
    match value {
        ActiveValue::Set(v) | ActiveValue::Unchanged(v) => *v,
        _ => fallback,
    }
}

fn apply_form_to_active_model(
    active: &mut BillTemplateActiveModel,
    form: &BillTemplatePayload,
    now: DateTime<Utc>,
    is_update: bool,
) -> Result<(), Response> {
    if !is_update {
        let name = form.name.clone().unwrap_or_default();
        active.name = Set(name);
        active.currency = Set(form.currency.clone().unwrap_or("USD".into()));
        active.billing_interval = Set(form.billing_interval.unwrap_or(BillingInterval::Monthly));

        let initial_increment = normalize_increment_value(
            form.initial_increment_secs.unwrap_or(60),
            "initial_increment_secs",
        )?;
        let billing_increment = normalize_increment_value(
            form.billing_increment_secs.unwrap_or(initial_increment),
            "billing_increment_secs",
        )?;
        validate_billing_pair(initial_increment, billing_increment)?;

        active.included_minutes = Set(form.included_minutes.unwrap_or(0));
        active.included_messages = Set(form.included_messages.unwrap_or(0));
        active.initial_increment_secs = Set(initial_increment);
        active.billing_increment_secs = Set(billing_increment);
        active.overage_rate_per_minute = Set(form.overage_rate_per_minute.unwrap_or(0.0));
        active.setup_fee = Set(form.setup_fee.unwrap_or(0.0));
        active.tax_percent = Set(form.tax_percent.unwrap_or(0.0));
        active.created_at = Set(now);
    } else {
        if let Some(name) = &form.name {
            active.name = Set(name.clone());
        }
        if let Some(currency) = &form.currency {
            active.currency = Set(currency.clone());
        }
        if let Some(interval) = form.billing_interval {
            active.billing_interval = Set(interval);
        }
        if form.included_minutes.is_some() {
            active.included_minutes = Set(form.included_minutes.unwrap_or(0));
        }
        if form.included_messages.is_some() {
            active.included_messages = Set(form.included_messages.unwrap_or(0));
        }
        let current_initial = active_value_i32(&active.initial_increment_secs, 60);
        let current_billing = active_value_i32(&active.billing_increment_secs, current_initial);
        let mut desired_initial = current_initial;
        let mut desired_billing = current_billing;
        let mut touched = false;

        if let Some(initial) = form.initial_increment_secs {
            desired_initial = normalize_increment_value(initial, "initial_increment_secs")?;
            touched = true;
        }
        if let Some(increment) = form.billing_increment_secs {
            desired_billing = normalize_increment_value(increment, "billing_increment_secs")?;
            touched = true;
        }

        if touched {
            validate_billing_pair(desired_initial, desired_billing)?;
            active.initial_increment_secs = Set(desired_initial);
            active.billing_increment_secs = Set(desired_billing);
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
        active.display_name = Set(form.display_name.clone());
    }
    if !is_update || form.description.is_some() {
        active.description = Set(form.description.clone());
    }
    if !is_update || form.metadata.is_some() {
        active.metadata = Set(form
            .metadata
            .as_ref()
            .map(|s| serde_json::from_str(s).unwrap_or(json!({}))));
    }

    active.updated_at = Set(now);

    Ok(())
}
