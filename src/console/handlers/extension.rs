use crate::addons::queue::models::{Column as QueueColumn, Entity as QueueEntity};
use crate::addons::queue::services::utils as queue_utils;
use crate::call::Location;
use crate::console::handlers::forms::{self, ExtensionPayload, ListQuery};
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::{
    department::{Column as DepartmentColumn, Entity as DepartmentEntity},
    extension::{
        ActiveModel as ExtensionActiveModel, Column as ExtensionColumn, Entity as ExtensionEntity,
    },
    extension_department::{
        Column as ExtensionDepartmentColumn, Entity as ExtensionDepartmentEntity,
    },
};
use crate::proxy::server::SipServerRef;
use axum::routing::get;
use axum::{Json, Router};
use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, NaiveDateTime, Utc};
use rsip::Uri;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, Order, Query as SeaQuery};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, ModelTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tracing::warn;

#[derive(Debug, Clone, Deserialize, Default)]
struct QueryExtensionsFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    department_ids: Option<Vec<i64>>,
    #[serde(default)]
    call_forwarding_enabled: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    login_allowed: Option<bool>,
    #[serde(default)]
    created_at_from: Option<String>,
    #[serde(default)]
    created_at_to: Option<String>,
    #[serde(default)]
    registered_at_from: Option<String>,
    #[serde(default)]
    registered_at_to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ForwardingCatalog {
    queues: Vec<ForwardingQueue>,
}

impl ForwardingCatalog {
    fn empty() -> Self {
        Self { queues: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ForwardingQueue {
    id: i64,
    name: String,
    reference: String,
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ExtensionLocatorRecord {
    binding_key: String,
    aor: String,
    destination: Option<String>,
    expires: u32,
    supports_webrtc: bool,
    contact: Option<String>,
    registered_aor: Option<String>,
    gruu: Option<String>,
    temp_gruu: Option<String>,
    instance_id: Option<String>,
    transport: Option<String>,
    path: Vec<String>,
    service_route: Vec<String>,
    contact_params: Option<HashMap<String, String>>,
    age_seconds: Option<u64>,
    user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ExtensionLocatorSummary {
    available: bool,
    query_uri: Option<String>,
    realm: Option<String>,
    total: usize,
    records: Vec<ExtensionLocatorRecord>,
    error: Option<String>,
}

fn parse_datetime_filter(value: &str) -> Option<DateTime<Utc>> {
    if value.trim().is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
    }
    None
}

fn resolve_default_realm(state: &ConsoleState) -> String {
    if let Some(app) = state.app_state() {
        return app.config().proxy.select_realm("");
    }
    "localhost".to_string()
}

async fn fetch_extension_locator_summary(
    server: Option<SipServerRef>,
    realm: &str,
    extension: &str,
) -> ExtensionLocatorSummary {
    let mut summary = ExtensionLocatorSummary {
        realm: Some(realm.to_string()),
        ..Default::default()
    };

    let trimmed_ext = extension.trim();
    if trimmed_ext.is_empty() {
        summary.error = Some("Extension identifier missing".to_string());
        return summary;
    }

    let query_uri = if trimmed_ext.starts_with("sip:") {
        trimmed_ext.to_string()
    } else if trimmed_ext.contains('@') {
        format!("sip:{}", trimmed_ext)
    } else {
        format!("sip:{}@{}", trimmed_ext, realm)
    };
    summary.query_uri = Some(query_uri.clone());

    let uri = match Uri::try_from(query_uri.as_str()) {
        Ok(uri) => uri,
        Err(err) => {
            summary.error = Some(format!("Invalid SIP URI: {}", err));
            return summary;
        }
    };

    let Some(server) = server else {
        summary.error = Some("SIP server unavailable".to_string());
        return summary;
    };

    match server.locator.lookup(&uri).await {
        Ok(locations) => {
            let records = locations
                .into_iter()
                .map(location_to_record)
                .collect::<Vec<_>>();
            summary.total = records.len();
            summary.records = records;
            summary.available = true;
        }
        Err(err) => {
            warn!(
                "locator lookup failed for extension {} ({}): {}",
                trimmed_ext, query_uri, err
            );
            summary.error = Some(format!("Locator lookup failed: {}", err));
        }
    }

    summary
}

fn location_to_record(location: Location) -> ExtensionLocatorRecord {
    let binding_key = location.binding_key();
    let aor = location.aor.to_string();
    let destination = location.destination.as_ref().map(|dest| dest.to_string());
    let registered_aor = location.registered_aor.as_ref().map(|uri| uri.to_string());
    let transport = location.transport.as_ref().map(|t| t.to_string());
    let path = location
        .path
        .as_ref()
        .into_iter()
        .flat_map(|vec| vec.iter())
        .map(|uri| uri.to_string())
        .collect::<Vec<_>>();
    let service_route = location
        .service_route
        .as_ref()
        .into_iter()
        .flat_map(|vec| vec.iter())
        .map(|uri| uri.to_string())
        .collect::<Vec<_>>();
    let contact_params = location.contact_params.clone();
    let contact = location.contact_raw.clone();
    let gruu = location.gruu.clone();
    let temp_gruu = location.temp_gruu.clone();
    let instance_id = location.instance_id.clone();
    let user_agent = location.user_agent.clone();
    let supports_webrtc = location.supports_webrtc;
    let expires = location.expires;
    let age_seconds = location
        .last_modified
        .map(|instant| instant.elapsed().as_secs());

    ExtensionLocatorRecord {
        binding_key,
        aor,
        destination,
        expires,
        supports_webrtc,
        contact,
        registered_aor,
        gruu,
        temp_gruu,
        instance_id,
        transport,
        path,
        service_route,
        contact_params,
        age_seconds,
        user_agent,
    }
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route(
            "/extensions",
            get(page_extensions)
                .put(create_extension)
                .post(query_extensions),
        )
        .route("/extensions/new", get(page_extension_create))
        .route(
            "/extensions/{id}",
            get(page_extension_detail)
                .patch(update_extension)
                .delete(delete_extension),
        )
}

async fn build_filters(state: Arc<ConsoleState>) -> serde_json::Value {
    let departments = match DepartmentEntity::find()
        .order_by_asc(DepartmentColumn::Name)
        .all(state.db())
        .await
    {
        Ok(departments) => departments,
        Err(err) => {
            warn!("failed to load departments: {}", err);
            vec![]
        }
    };

    let statuses = match ExtensionEntity::find()
        .select_only()
        .column(ExtensionColumn::Status)
        .distinct()
        .into_tuple::<Option<String>>()
        .all(state.db())
        .await
    {
        Ok(results) => results
            .into_iter()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<String>>(),
        Err(err) => {
            warn!("failed to load extension statuses: {}", err);
            vec![]
        }
    };

    return json!({
        "departments": departments,
        "statuses": statuses,
    });
}

async fn build_forwarding_catalog(state: Arc<ConsoleState>) -> ForwardingCatalog {
    let mut catalog = ForwardingCatalog::empty();
    let queues = match QueueEntity::find()
        .order_by_asc(QueueColumn::Name)
        .all(state.db())
        .await
    {
        Ok(models) => models,
        Err(err) => {
            warn!("failed to load queues for forwarding catalog: {}", err);
            vec![]
        }
    };
    catalog.queues = queues
        .into_iter()
        .filter_map(|queue| match queue_utils::convert_queue_model(queue) {
            Ok(entry) => {
                let Some(id) = entry.id else {
                    warn!("queue entry missing id when building forwarding catalog");
                    return None;
                };
                Some(ForwardingQueue {
                    id,
                    reference: id.to_string(),
                    name: entry.name,
                    description: entry.description,
                })
            }
            Err(err) => {
                warn!(error = %err, "failed to convert queue when building forwarding catalog");
                None
            }
        })
        .collect();
    catalog
}

async fn page_extensions(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/extensions.html",
        json!({
            "nav_active": "extensions",
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
        }),
    )
}

async fn page_extension_detail(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();

    let (model, departments) = match ExtensionEntity::find_by_id_with_departments(db, id).await {
        Ok(Some((model, departments))) => (model, departments),
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Extension not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load extension {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let realm = resolve_default_realm(state.as_ref());
    let locator_info =
        fetch_extension_locator_summary(state.sip_server(), &realm, &model.extension).await;
    let forwarding_catalog = build_forwarding_catalog(state.clone()).await;

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "model": model,
            "departments": departments,
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
            "registration_info": locator_info,
            "forwarding_catalog": forwarding_catalog,
            "addon_scripts": state.get_injected_scripts(&format!("/console/extensions/{}", model.id)),
        }),
    )
}

async fn page_extension_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let forwarding_catalog = build_forwarding_catalog(state.clone()).await;
    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
            "registration_info": ExtensionLocatorSummary::default(),
            "forwarding_catalog": forwarding_catalog,
            "addon_scripts": state.get_injected_scripts("/console/extensions/new"),
        }),
    )
}

async fn create_extension(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<ExtensionPayload>,
) -> Response {
    let db = state.db();
    let extension = match payload.extension {
        Some(ref ext) if !ext.is_empty() => ext,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message": "Extension is required"})),
            )
                .into_response();
        }
    };
    let now = chrono::Utc::now();
    let active = ExtensionActiveModel {
        extension: Set(extension.clone()),
        display_name: Set(payload.display_name),
        email: Set(payload.email),
        sip_password: Set(payload.sip_password),
        call_forwarding_mode: Set(payload.call_forwarding_mode),
        call_forwarding_destination: Set(payload.call_forwarding_destination),
        call_forwarding_timeout: Set(payload.call_forwarding_timeout),
        login_disabled: Set(payload.login_disabled.unwrap_or(false)),
        voicemail_disabled: Set(payload.voicemail_disabled.unwrap_or(false)),
        allow_guest_calls: Set(payload.allow_guest_calls.unwrap_or(false)),
        notes: Set(payload.notes),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    let model = match active.insert(db).await {
        Ok(model) => model,
        Err(err) => {
            warn!("failed to create extension: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    if let Some(ref dept_ids) = payload.department_ids {
        if let Err(err) = ExtensionEntity::replace_departments(db, model.id, dept_ids).await {
            warn!(
                "failed to set departments for extension {}: {}",
                model.id, err
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    }

    Json(json!({"status": "ok", "id": model.id})).into_response()
}

async fn query_extensions(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<ListQuery<QueryExtensionsFilters>>,
) -> Response {
    let db = state.db();
    let (_, per_page) = payload.normalize();

    let mut selector = ExtensionEntity::find();
    if let Some(filters) = &payload.filters {
        if let Some(ref q_raw) = filters.q {
            let trimmed = q_raw.trim();
            if !trimmed.is_empty() {
                let mut condition = Condition::any();
                condition = condition.add(ExtensionColumn::Extension.contains(trimmed));
                condition = condition.add(ExtensionColumn::DisplayName.contains(trimmed));
                condition = condition.add(ExtensionColumn::Email.contains(trimmed));
                selector = selector.filter(condition);
            }
        }

        if let Some(mut department_ids) = filters.department_ids.clone() {
            department_ids.sort_unstable();
            department_ids.dedup();

            if !department_ids.is_empty() {
                let mut subquery = SeaQuery::select();
                subquery
                    .column(ExtensionDepartmentColumn::ExtensionId)
                    .from(ExtensionDepartmentEntity)
                    .and_where(
                        Expr::col(ExtensionDepartmentColumn::DepartmentId)
                            .is_in(department_ids.clone()),
                    )
                    .group_by_col(ExtensionDepartmentColumn::ExtensionId)
                    .and_having(
                        Expr::col(ExtensionDepartmentColumn::DepartmentId)
                            .count_distinct()
                            .eq(department_ids.len() as i32),
                    );

                selector = selector.filter(ExtensionColumn::Id.in_subquery(subquery.to_owned()));
            }
        }

        if let Some(call_forwarding_enabled) = filters.call_forwarding_enabled {
            if call_forwarding_enabled {
                let mut condition = Condition::all();
                condition = condition.add(ExtensionColumn::CallForwardingMode.is_not_null());
                condition = condition.add(ExtensionColumn::CallForwardingMode.ne("none"));
                selector = selector.filter(condition);
            } else {
                let mut condition = Condition::any();
                condition = condition.add(ExtensionColumn::CallForwardingMode.is_null());
                condition = condition.add(ExtensionColumn::CallForwardingMode.eq("none"));
                selector = selector.filter(condition);
            }
        }

        if let Some(ref status) = filters.status {
            let trimmed = status.trim();
            if !trimmed.is_empty() {
                selector = selector.filter(ExtensionColumn::Status.eq(trimmed));
            }
        }

        if let Some(login_allowed) = filters.login_allowed {
            selector = selector.filter(ExtensionColumn::LoginDisabled.eq(!login_allowed));
        }

        if let Some(ref from_raw) = filters.created_at_from {
            if let Some(from) = parse_datetime_filter(from_raw) {
                selector = selector.filter(ExtensionColumn::CreatedAt.gte(from));
            }
        }

        if let Some(ref to_raw) = filters.created_at_to {
            if let Some(to) = parse_datetime_filter(to_raw) {
                selector = selector.filter(ExtensionColumn::CreatedAt.lte(to));
            }
        }

        if let Some(ref from_raw) = filters.registered_at_from {
            if let Some(from) = parse_datetime_filter(from_raw) {
                selector = selector.filter(ExtensionColumn::RegisteredAt.gte(from));
            }
        }

        if let Some(ref to_raw) = filters.registered_at_to {
            if let Some(to) = parse_datetime_filter(to_raw) {
                selector = selector.filter(ExtensionColumn::RegisteredAt.lte(to));
            }
        }
    }
    let sort_key = payload.sort.as_deref().unwrap_or("created_at_desc");
    match sort_key {
        "created_at_asc" => {
            selector = selector.order_by(ExtensionColumn::CreatedAt, Order::Asc);
        }
        "extension_asc" => {
            selector = selector.order_by(ExtensionColumn::Extension, Order::Asc);
        }
        "extension_desc" => {
            selector = selector.order_by(ExtensionColumn::Extension, Order::Desc);
        }
        "display_name_asc" => {
            selector = selector.order_by(ExtensionColumn::DisplayName, Order::Asc);
        }
        "display_name_desc" => {
            selector = selector.order_by(ExtensionColumn::DisplayName, Order::Desc);
        }
        "status_asc" => {
            selector = selector.order_by(ExtensionColumn::Status, Order::Asc);
        }
        "status_desc" => {
            selector = selector.order_by(ExtensionColumn::Status, Order::Desc);
        }
        "registered_at_asc" => {
            selector = selector.order_by(ExtensionColumn::RegisteredAt, Order::Asc);
        }
        "registered_at_desc" => {
            selector = selector.order_by(ExtensionColumn::RegisteredAt, Order::Desc);
        }
        "created_at_desc" => {
            selector = selector.order_by(ExtensionColumn::CreatedAt, Order::Desc);
        }
        _ => {
            selector = selector.order_by(ExtensionColumn::CreatedAt, Order::Desc);
        }
    }
    selector = selector.order_by(ExtensionColumn::Id, Order::Desc);

    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, &payload).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to query extensions: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let mut items = vec![];
    let realm = resolve_default_realm(state.as_ref());
    let server = state.sip_server();

    for ext in &pagination.items {
        let departments = ext
            .find_related(crate::models::department::Entity)
            .all(db)
            .await
            .ok();
        let registrations =
            fetch_extension_locator_summary(server.clone(), &realm, &ext.extension).await;
        items.push(json!({
            "extension": ext,
            "departments": departments,
            "registrations": registrations,
        }));
    }
    let result = json!({
        "page": pagination.current_page,
        "per_page": pagination.per_page,
        "total_pages": pagination.total_pages,
        "total_items": pagination.total_items,
        "items": items,
    });
    Json(result).into_response()
}

async fn update_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<ExtensionPayload>,
) -> Response {
    let db = state.db();
    let model = match ExtensionEntity::find_by_id(id).one(db).await {
        Ok(Some(result)) => result,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Extension not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load extension {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let mut active: ExtensionActiveModel = model.into();
    if let Some(extension) = payload.extension {
        active.extension = Set(extension);
    }
    if let Some(display_name) = payload.display_name {
        active.display_name = Set(Some(display_name));
    }
    if let Some(email) = payload.email {
        active.email = Set(Some(email));
    }
    if let Some(sip_password) = payload.sip_password {
        active.sip_password = Set(Some(sip_password));
    }
    if let Some(call_forwarding_mode) = payload.call_forwarding_mode {
        active.call_forwarding_mode = Set(Some(call_forwarding_mode));
    }
    if let Some(call_forwarding_destination) = payload.call_forwarding_destination {
        active.call_forwarding_destination = Set(Some(call_forwarding_destination));
    }
    if let Some(call_forwarding_timeout) = payload.call_forwarding_timeout {
        active.call_forwarding_timeout = Set(Some(call_forwarding_timeout));
    }
    if let Some(notes) = payload.notes {
        active.notes = Set(Some(notes));
    }
    if let Some(login_disabled) = payload.login_disabled {
        active.login_disabled = Set(login_disabled);
    }
    if let Some(voicemail_disabled) = payload.voicemail_disabled {
        active.voicemail_disabled = Set(voicemail_disabled);
    }
    if let Some(allow_guest_calls) = payload.allow_guest_calls {
        active.allow_guest_calls = Set(allow_guest_calls);
    }
    active.updated_at = Set(chrono::Utc::now());

    if let Err(err) = active.update(db).await {
        warn!("failed to update extension {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": err.to_string()})),
        )
            .into_response();
    }
    if let Some(ref dept_ids) = payload.department_ids {
        if let Err(err) = ExtensionEntity::replace_departments(db, id, dept_ids).await {
            warn!("failed to update departments for extension {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    }
    Json(json!({"status": "ok"})).into_response()
}

async fn delete_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    match ExtensionEntity::delete_by_id(id).exec(state.db()).await {
        Ok(r) => Json(json!({"status": r.rows_affected})).into_response(),
        Err(err) => {
            warn!("failed to delete extension {}: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConsoleConfig,
        console::handlers::forms::ListQuery,
        models::{department, extension::Model as ExtensionModel, migration::Migrator, user},
    };
    use axum::{Json, body::to_bytes, extract::State, http::StatusCode};
    use chrono::Utc;
    use sea_orm::{ActiveValue::Set, Database};
    use sea_orm_migration::MigratorTrait;
    use serde_json::Value;
    use std::sync::Arc;

    fn dummy_user() -> user::Model {
        let now = Utc::now();
        user::Model {
            id: 1,
            email: "tester@rustpbx.com".into(),
            username: "tester".into(),
            password_hash: "hashed".into(),
            reset_token: None,
            reset_token_expires: None,
            last_login_at: None,
            last_login_ip: None,
            created_at: now,
            updated_at: now,
            is_active: true,
            is_staff: true,
            is_superuser: true,
        }
    }

    async fn setup_state() -> Arc<ConsoleState> {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        ConsoleState::initialize(
            Arc::new(crate::callrecord::DefaultCallRecordFormatter::default()),
            db,
            ConsoleConfig::default(),
        )
        .await
        .expect("initialize console state")
    }

    async fn insert_extension(db: &sea_orm::DatabaseConnection, extension: &str) -> ExtensionModel {
        ExtensionActiveModel {
            extension: Set(extension.to_string()),
            login_disabled: Set(false),
            voicemail_disabled: Set(false),
            allow_guest_calls: Set(false),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert extension")
    }

    #[tokio::test]
    async fn query_extensions_filters_by_department() {
        let state = setup_state().await;
        let db = state.db();

        let sales = department::ActiveModel {
            name: Set("Sales".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert sales department");

        let support = department::ActiveModel {
            name: Set("Support".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert support department");

        let ext_sales = insert_extension(db, "1001").await;
        let ext_support = insert_extension(db, "2002").await;
        let ext_both = insert_extension(db, "3003").await;

        ExtensionEntity::replace_departments(db, ext_sales.id, &[sales.id])
            .await
            .expect("map sales department");
        ExtensionEntity::replace_departments(db, ext_support.id, &[support.id])
            .await
            .expect("map support department");
        ExtensionEntity::replace_departments(db, ext_both.id, &[sales.id, support.id])
            .await
            .expect("map both departments");

        async fn fetch_extension_ids(state: Arc<ConsoleState>, dept_ids: Vec<i64>) -> Vec<i64> {
            let mut query: ListQuery<QueryExtensionsFilters> = Default::default();
            query.filters = Some(QueryExtensionsFilters {
                department_ids: Some(dept_ids),
                ..Default::default()
            });

            let response =
                query_extensions(State(state), AuthRequired(dummy_user()), Json(query)).await;

            assert_eq!(response.status(), StatusCode::OK);

            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            let parsed: Value = serde_json::from_slice(&body).expect("parse json");
            parsed["items"]
                .as_array()
                .expect("items as array")
                .iter()
                .map(|item| {
                    item["extension"]["id"]
                        .as_i64()
                        .expect("extension id as i64")
                })
                .collect()
        }

        let mut sales_ids = fetch_extension_ids(state.clone(), vec![sales.id]).await;
        sales_ids.sort_unstable();
        assert_eq!(sales_ids, vec![ext_sales.id, ext_both.id]);

        let mut support_ids = fetch_extension_ids(state.clone(), vec![support.id]).await;
        support_ids.sort_unstable();
        assert_eq!(support_ids, vec![ext_support.id, ext_both.id]);

        let combined_ids = fetch_extension_ids(state.clone(), vec![sales.id, support.id]).await;
        assert_eq!(combined_ids, vec![ext_both.id]);
    }
}
