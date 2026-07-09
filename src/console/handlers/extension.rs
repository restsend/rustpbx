use crate::call::Location;
use crate::console::config_helpers::{bad_request, find_or_404, internal_error};
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
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, NaiveDateTime, Utc};
use rsipstack::sip::Uri;
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
    home_proxy: Option<String>,
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

    let query_uri = crate::call::build_sip_uri(trimmed_ext, realm);
    summary.query_uri = Some(query_uri.clone());

    let Some(server) = server else {
        summary.error = Some("SIP server unavailable".to_string());
        return summary;
    };

    let mut lookup_uris = vec![query_uri.clone()];
    let localhost_query_uri = format!("sip:{}@localhost", trimmed_ext);
    if !query_uri.eq_ignore_ascii_case(&localhost_query_uri) {
        lookup_uris.push(localhost_query_uri);
    }

    let mut had_success = false;
    let mut last_error = None;

    for candidate in lookup_uris {
        let uri = match Uri::try_from(candidate.as_str()) {
            Ok(uri) => uri,
            Err(err) => {
                last_error = Some(format!("Invalid SIP URI: {}", err));
                continue;
            }
        };

        match server.locator.lookup(&uri).await {
            Ok(locations) => {
                had_success = true;
                if locations.is_empty() {
                    continue;
                }

                let records = locations
                    .into_iter()
                    .map(location_to_record)
                    .collect::<Vec<_>>();
                summary.total = records.len();
                summary.records = records;
                summary.available = true;
                summary.query_uri = Some(candidate);
                return summary;
            }
            Err(err) => {
                warn!(
                    "locator lookup failed for extension {} ({}): {}",
                    trimmed_ext, candidate, err
                );
                last_error = Some(format!("Locator lookup failed: {}", err));
            }
        }
    }

    summary.available = true;
    if !had_success {
        summary.error = last_error;
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
    let home_proxy = location.home_proxy.map(|h| h.to_string());

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
        home_proxy,
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

pub fn api_urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/extensions", put(create_extension).post(query_extensions))
        .route(
            "/extensions/{id}",
            patch(update_extension).delete(delete_extension),
        )
        .route("/extensions/import", post(csv_import_extensions))
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

    json!({
        "departments": departments,
        "statuses": statuses,
    })
}

fn build_forwarding_catalog(state: &ConsoleState) -> crate::console::catalog::ForwardingCatalog {
    if let Some(proxy_config) =
        crate::console::catalog::load_proxy_config(state.app_state().as_ref())
    {
        crate::console::catalog::build_forwarding_catalog(&proxy_config)
    } else {
        crate::console::catalog::ForwardingCatalog::empty()
    }
}

async fn page_extensions(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let current_user = state.build_current_user_ctx(&user).await;
    state.render_with_headers(
        "console/extensions.html",
        json!({
            "nav_active": "extensions",
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
            "current_user": current_user,
        }),
        &headers,
    )
}

async fn page_extension_detail(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
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
    let forwarding_catalog = build_forwarding_catalog(&state);
    let current_user = state.build_current_user_ctx(&user).await;

    state.render_with_headers(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "model": model,
            "departments": departments,
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
            "registration_info": locator_info,
            "forwarding_catalog": forwarding_catalog,
            "addon_scripts": state.get_injected_scripts(&format!("{}/extensions/{}", state.base_path(), model.id)),
            "current_user": current_user,
        }),
        &headers,
    )
}

async fn page_extension_create(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let forwarding_catalog = build_forwarding_catalog(&state);
    let current_user = state.build_current_user_ctx(&user).await;
    state.render_with_headers(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
            "registration_info": ExtensionLocatorSummary::default(),
            "forwarding_catalog": forwarding_catalog,
            "addon_scripts": state.get_injected_scripts(&format!("{}/extensions/new", state.base_path())),
            "current_user": current_user,
        }),
        &headers,
    )
}

async fn create_extension(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    Json(payload): Json<ExtensionPayload>,
) -> Response {
    if let Err(resp) = state.require_permission(&user, "extensions", "write").await {
        return resp;
    }
    let db = state.db();
    let extension = match payload.extension {
        Some(ref ext) if !ext.is_empty() => ext,
        _ => {
            return bad_request("Extension is required");
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
            return internal_error(err.to_string());
        }
    };

    if let Some(ref dept_ids) = payload.department_ids
        && let Err(err) = ExtensionEntity::replace_departments(db, model.id, dept_ids).await
    {
        warn!(
            "failed to set departments for extension {}: {}",
            model.id, err
        );
        return internal_error(err.to_string());
    }

    // Notify addons that an extension was created.
    if let Some(app_state) = state.app_state() {
        app_state
            .addon_registry
            .on_extension_created(app_state.config(), db, extension)
            .await;
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

        if let Some(ref from_raw) = filters.created_at_from
            && let Some(from) = parse_datetime_filter(from_raw)
        {
            selector = selector.filter(ExtensionColumn::CreatedAt.gte(from));
        }

        if let Some(ref to_raw) = filters.created_at_to
            && let Some(to) = parse_datetime_filter(to_raw)
        {
            selector = selector.filter(ExtensionColumn::CreatedAt.lte(to));
        }

        if let Some(ref from_raw) = filters.registered_at_from
            && let Some(from) = parse_datetime_filter(from_raw)
        {
            selector = selector.filter(ExtensionColumn::RegisteredAt.gte(from));
        }

        if let Some(ref to_raw) = filters.registered_at_to
            && let Some(to) = parse_datetime_filter(to_raw)
        {
            selector = selector.filter(ExtensionColumn::RegisteredAt.lte(to));
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
    AuthRequired(user): AuthRequired,
    Json(payload): Json<ExtensionPayload>,
) -> Response {
    if let Err(resp) = state.require_permission(&user, "extensions", "write").await {
        return resp;
    }
    let db = state.db();
    let model = find_or_404!(ExtensionEntity, id, db, "Extension");

    // Capture the original extension before model is consumed.
    let current_ext = model.extension.clone();

    let mut active: ExtensionActiveModel = model.into();
    if let Some(ref extension) = payload.extension {
        active.extension = Set(extension.clone());
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
    if let Some(ref dept_ids) = payload.department_ids
        && let Err(err) = ExtensionEntity::replace_departments(db, id, dept_ids).await
    {
        warn!("failed to update departments for extension {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": err.to_string()})),
        )
            .into_response();
    }

    // Notify addons that an extension was updated.
    let ext = payload.extension.clone().unwrap_or(current_ext);
    if !ext.is_empty() {
        if let Some(app_state) = state.app_state() {
            app_state
                .addon_registry
                .on_extension_updated(app_state.config(), db, &ext)
                .await;
        }
    }

    Json(json!({"status": "ok"})).into_response()
}

async fn delete_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
) -> Response {
    if let Err(resp) = state
        .require_permission(&user, "extensions", "delete")
        .await
    {
        return resp;
    }
    let db = state.db();

    // Resolve extension number before deletion for addon hooks.
    let ext_number = ExtensionEntity::find_by_id(id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|m| m.extension)
        .unwrap_or_default();

    match ExtensionEntity::delete_by_id(id).exec(db).await {
        Ok(r) => {
            if !ext_number.is_empty() {
                if let Some(app_state) = state.app_state() {
                    app_state
                        .addon_registry
                        .on_extension_deleting(app_state.config(), db, &ext_number)
                        .await;
                }
            }
            Json(json!({"status": r.rows_affected})).into_response()
        }
        Err(err) => {
            warn!("failed to delete extension {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

fn parse_bool_string(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Some(true),
        "false" | "no" | "0" | "off" | "" => Some(false),
        _ => None,
    }
}

fn parse_i32_string(s: &str) -> Option<i32> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<i32>().ok()
}

#[derive(Deserialize, Default)]
pub struct CsvExtensionRow {
    pub extension: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub sip_password: Option<String>,
    #[serde(default)]
    pub login_disabled: Option<String>,
    #[serde(default)]
    pub voicemail_disabled: Option<String>,
    #[serde(default)]
    pub allow_guest_calls: Option<String>,
    #[serde(default)]
    pub call_forwarding_mode: Option<String>,
    #[serde(default)]
    pub call_forwarding_destination: Option<String>,
    #[serde(default)]
    pub call_forwarding_timeout: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub departments: Option<String>,
}

#[derive(Deserialize)]
pub struct CsvExtensionImportPayload {
    pub extensions: Vec<CsvExtensionRow>,
}

pub async fn csv_import_extensions(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    Json(payload): Json<CsvExtensionImportPayload>,
) -> Response {
    if let Err(resp) = state.require_permission(&user, "extensions", "write").await {
        return resp;
    }
    let db = state.db();
    let now = Utc::now();

    let mut existing_extensions: std::collections::HashSet<String> = ExtensionEntity::find()
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.extension)
        .collect();

    let all_depts = DepartmentEntity::find().all(db).await.unwrap_or_default();
    let dept_map: std::collections::HashMap<String, i64> =
        all_depts.iter().map(|d| (d.name.clone(), d.id)).collect();

    let mut imported = 0u32;
    let mut skipped = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for (i, row) in payload.extensions.iter().enumerate() {
        let ext_number = row.extension.trim().to_string();

        if ext_number.is_empty() {
            skipped += 1;
            errors.push(format!("Row {}: extension number is required", i + 1));
            continue;
        }

        if ext_number.len() > 32 {
            skipped += 1;
            errors.push(format!(
                "Row {}: extension '{}' exceeds maximum length of 32 characters",
                i + 1,
                ext_number
            ));
            continue;
        }

        if existing_extensions.contains(&ext_number) {
            skipped += 1;
            errors.push(format!(
                "Row {}: extension '{}' already exists",
                i + 1,
                ext_number
            ));
            continue;
        }

        let validate_bool = |val: &Option<String>| -> Option<bool> {
            val.as_deref()
                .filter(|s| !s.trim().is_empty())
                .and_then(parse_bool_string)
        };

        let login_disabled = validate_bool(&row.login_disabled).unwrap_or(false);
        let voicemail_disabled = validate_bool(&row.voicemail_disabled).unwrap_or(false);
        let allow_guest_calls = validate_bool(&row.allow_guest_calls).unwrap_or(false);

        if let Some(ref raw) = row.login_disabled {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && parse_bool_string(trimmed).is_none() {
                skipped += 1;
                errors.push(format!(
                    "Row {}: extension '{}' has invalid login_disabled value '{}'",
                    i + 1,
                    ext_number,
                    raw
                ));
                continue;
            }
        }
        if let Some(ref raw) = row.voicemail_disabled {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && parse_bool_string(trimmed).is_none() {
                skipped += 1;
                errors.push(format!(
                    "Row {}: extension '{}' has invalid voicemail_disabled value '{}'",
                    i + 1,
                    ext_number,
                    raw
                ));
                continue;
            }
        }
        if let Some(ref raw) = row.allow_guest_calls {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && parse_bool_string(trimmed).is_none() {
                skipped += 1;
                errors.push(format!(
                    "Row {}: extension '{}' has invalid allow_guest_calls value '{}'",
                    i + 1,
                    ext_number,
                    raw
                ));
                continue;
            }
        }

        let call_forwarding_timeout = row
            .call_forwarding_timeout
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(parse_i32_string);

        if let Some(ref raw) = row.call_forwarding_timeout {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && parse_i32_string(trimmed).is_none() {
                skipped += 1;
                errors.push(format!(
                    "Row {}: extension '{}' has invalid call_forwarding_timeout value '{}'",
                    i + 1,
                    ext_number,
                    raw
                ));
                continue;
            }
        }

        let display_name = row.display_name.clone().filter(|s| !s.is_empty());
        let email = row.email.clone().filter(|s| !s.is_empty());
        let sip_password = row.sip_password.clone().filter(|s| !s.is_empty());
        let call_forwarding_mode = row.call_forwarding_mode.clone().filter(|s| !s.is_empty());
        let call_forwarding_destination = row
            .call_forwarding_destination
            .clone()
            .filter(|s| !s.is_empty());
        let notes = row.notes.clone().filter(|s| !s.is_empty());

        let dept_names: Vec<&str> = row
            .departments
            .as_deref()
            .map(|d| {
                d.split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let mut dept_ids = Vec::new();
        let mut unknown_depts = Vec::new();
        for name in &dept_names {
            if let Some(&id) = dept_map.get(*name) {
                dept_ids.push(id);
            } else {
                unknown_depts.push(*name);
            }
        }
        if !unknown_depts.is_empty() {
            skipped += 1;
            errors.push(format!(
                "Row {}: extension '{}' has unknown departments: {}",
                i + 1,
                ext_number,
                unknown_depts.join(", ")
            ));
            continue;
        }

        let active = ExtensionActiveModel {
            extension: Set(ext_number.clone()),
            display_name: Set(display_name),
            email: Set(email),
            sip_password: Set(sip_password),
            login_disabled: Set(login_disabled),
            voicemail_disabled: Set(voicemail_disabled),
            allow_guest_calls: Set(allow_guest_calls),
            call_forwarding_mode: Set(call_forwarding_mode),
            call_forwarding_destination: Set(call_forwarding_destination),
            call_forwarding_timeout: Set(call_forwarding_timeout),
            notes: Set(notes),
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
            registered_at: Set(None),
            status: Set(None),
            ..Default::default()
        };

        match active.insert(db).await {
            Ok(model) => {
                if !dept_ids.is_empty() {
                    if let Err(err) =
                        ExtensionEntity::replace_departments(db, model.id, &dept_ids).await
                    {
                        errors.push(format!(
                            "Row {}: extension '{}' imported but department assignment failed: {}",
                            i + 1,
                            ext_number,
                            err
                        ));
                    }
                }
                // Notify addons that an extension was created via CSV import.
                if let Some(app_state) = state.app_state() {
                    app_state
                        .addon_registry
                        .on_extension_created(app_state.config(), db, &ext_number)
                        .await;
                }
                imported += 1;
                existing_extensions.insert(ext_number);
            }
            Err(e) => {
                skipped += 1;
                errors.push(format!(
                    "Row {}: extension '{}' failed to insert: {}",
                    i + 1,
                    ext_number,
                    e
                ));
            }
        }
    }

    Json(json!({
        "status": if imported > 0 { "ok" } else { "error" },
        "imported": imported,
        "skipped": skipped,
        "errors": errors,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use crate::console::handlers::test_helpers::{setup_state, superuser, unprivileged_user};

    use super::*;
    use crate::{
        console::handlers::forms::ListQuery,
        models::{department, extension::Model as ExtensionModel},
    };
    use axum::{Json, body::to_bytes, extract::State, http::StatusCode};
    use sea_orm::ActiveValue::Set;
    use serde_json::Value;
    use std::sync::Arc;

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
                query_extensions(State(state), AuthRequired(superuser()), Json(query)).await;

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

    #[tokio::test]
    async fn create_extension_denied_without_permission() {
        let state = setup_state().await;
        let user = unprivileged_user();
        let payload = ExtensionPayload {
            extension: Some("5000".into()),
            ..Default::default()
        };
        let resp = create_extension(State(state), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn update_extension_denied_without_permission() {
        let state = setup_state().await;
        let ext = insert_extension(state.db(), "5001").await;
        let user = unprivileged_user();
        let payload = ExtensionPayload {
            extension: Some("5001".into()),
            ..Default::default()
        };
        let resp = update_extension(
            AxumPath(ext.id),
            State(state),
            AuthRequired(user),
            Json(payload),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_extension_denied_without_permission() {
        let state = setup_state().await;
        let ext = insert_extension(state.db(), "5002").await;
        let user = unprivileged_user();
        let resp = delete_extension(AxumPath(ext.id), State(state), AuthRequired(user)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_extension_allowed_for_superuser() {
        let state = setup_state().await;
        let user = superuser();
        let payload = ExtensionPayload {
            extension: Some("5003".into()),
            ..Default::default()
        };
        let resp = create_extension(State(state), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn csv_import_extensions_basic() {
        let state = setup_state().await;
        let user = superuser();
        let payload = CsvExtensionImportPayload {
            extensions: vec![
                CsvExtensionRow {
                    extension: "2001".into(),
                    display_name: Some("Alice".into()),
                    email: Some("alice@test.com".into()),
                    sip_password: Some("secret123".into()),
                    login_disabled: Some("false".into()),
                    voicemail_disabled: Some("false".into()),
                    allow_guest_calls: Some("true".into()),
                    call_forwarding_mode: Some("".into()),
                    call_forwarding_destination: Some("".into()),
                    call_forwarding_timeout: Some("".into()),
                    notes: Some("test".into()),
                    departments: Some("".into()),
                },
                CsvExtensionRow {
                    extension: "2002".into(),
                    display_name: Some("Bob".into()),
                    email: Some("".into()),
                    sip_password: Some("".into()),
                    login_disabled: Some("true".into()),
                    voicemail_disabled: Some("true".into()),
                    allow_guest_calls: Some("false".into()),
                    call_forwarding_mode: Some("always".into()),
                    call_forwarding_destination: Some("1001".into()),
                    call_forwarding_timeout: Some("30".into()),
                    notes: Some("".into()),
                    departments: Some("".into()),
                },
            ],
        };
        let resp =
            csv_import_extensions(State(state.clone()), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: Value = serde_json::from_slice(&body).expect("parse json");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["imported"], 2);
        assert_eq!(parsed["skipped"], 0);

        let extensions = ExtensionEntity::find().all(state.db()).await.unwrap();
        let ext_names: Vec<&str> = extensions.iter().map(|e| e.extension.as_str()).collect();
        assert!(ext_names.contains(&"2001"));
        assert!(ext_names.contains(&"2002"));
    }

    #[tokio::test]
    async fn csv_import_extensions_duplicate() {
        let state = setup_state().await;
        insert_extension(state.db(), "3001").await;
        let user = superuser();
        let payload = CsvExtensionImportPayload {
            extensions: vec![
                CsvExtensionRow {
                    extension: "3001".into(),
                    ..Default::default()
                },
                CsvExtensionRow {
                    extension: "3002".into(),
                    ..Default::default()
                },
            ],
        };
        let resp =
            csv_import_extensions(State(state.clone()), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: Value = serde_json::from_slice(&body).expect("parse json");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["imported"], 1);
        assert_eq!(parsed["skipped"], 1);
        let errors = parsed["errors"].as_array().unwrap();
        assert!(errors[0].as_str().unwrap().contains("already exists"));
    }

    #[tokio::test]
    async fn csv_import_extensions_empty_extension() {
        let state = setup_state().await;
        let user = superuser();
        let payload = CsvExtensionImportPayload {
            extensions: vec![CsvExtensionRow {
                extension: "".into(),
                ..Default::default()
            }],
        };
        let resp = csv_import_extensions(State(state), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: Value = serde_json::from_slice(&body).expect("parse json");
        assert_eq!(parsed["status"], "error");
        assert_eq!(parsed["imported"], 0);
        assert_eq!(parsed["skipped"], 1);
        assert!(
            parsed["errors"][0]
                .as_str()
                .unwrap()
                .contains("extension number is required")
        );
    }

    #[tokio::test]
    async fn csv_import_extensions_with_departments() {
        let state = setup_state().await;
        let db = state.db();
        let _ = department::ActiveModel {
            name: Set("Sales".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert sales");
        let _ = department::ActiveModel {
            name: Set("Support".to_string()),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert support");

        let user = superuser();
        let payload = CsvExtensionImportPayload {
            extensions: vec![CsvExtensionRow {
                extension: "4001".into(),
                display_name: Some("Dept Test".into()),
                departments: Some("Sales,Support".into()),
                ..Default::default()
            }],
        };
        let resp =
            csv_import_extensions(State(state.clone()), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: Value = serde_json::from_slice(&body).expect("parse json");
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["imported"], 1);

        let ext = ExtensionEntity::find()
            .filter(ExtensionColumn::Extension.eq("4001"))
            .one(db)
            .await
            .unwrap()
            .expect("extension found");
        let ext_depts = ext
            .find_related(crate::models::department::Entity)
            .all(db)
            .await
            .unwrap();
        let dept_names: Vec<&str> = ext_depts.iter().map(|d| d.name.as_str()).collect();
        assert!(dept_names.contains(&"Sales"));
        assert!(dept_names.contains(&"Support"));
    }

    #[tokio::test]
    async fn csv_import_extensions_invalid_bool() {
        let state = setup_state().await;
        let user = superuser();
        let payload = CsvExtensionImportPayload {
            extensions: vec![CsvExtensionRow {
                extension: "5001".into(),
                login_disabled: Some("invalid".into()),
                ..Default::default()
            }],
        };
        let resp = csv_import_extensions(State(state), AuthRequired(user), Json(payload)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: Value = serde_json::from_slice(&body).expect("parse json");
        assert_eq!(parsed["status"], "error");
        assert_eq!(parsed["imported"], 0);
        assert_eq!(parsed["skipped"], 1);
        assert!(
            parsed["errors"][0]
                .as_str()
                .unwrap()
                .contains("invalid login_disabled")
        );
    }
}
