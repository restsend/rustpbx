use crate::app::AppStateInner;
use crate::config::{CallRecordConfig, ProxyConfig, ProxyDataSource, UserBackendConfig};
use crate::console::handlers::forms;
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::department::{
    ActiveModel as DepartmentActiveModel, Column as DepartmentColumn, Entity as DepartmentEntity,
};
use crate::models::user::{
    ActiveModel as UserActiveModel, Column as UserColumn, Entity as UserEntity, Model as UserModel,
};
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHasher, SaltString};
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use sea_orm::sea_query::Condition;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, Deserialize, Default)]
struct QueryDepartmentFilters {
    pub q: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct QueryUserFilters {
    pub q: Option<String>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct DepartmentPayload {
    pub name: String,
    pub display_label: Option<String>,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub manager_contact: Option<String>,
    #[serde(default)]
    pub metadata: Option<JsonValue>,
}

#[derive(Debug, Clone, Deserialize)]
struct UserPayload {
    pub email: String,
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub is_staff: Option<bool>,
    #[serde(default)]
    pub is_superuser: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct UserView {
    pub id: i64,
    pub email: String,
    pub username: String,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_login_ip: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
    pub is_staff: bool,
    pub is_superuser: bool,
}

impl From<UserModel> for UserView {
    fn from(model: UserModel) -> Self {
        Self {
            id: model.id,
            email: model.email,
            username: model.username,
            last_login_at: model.last_login_at,
            last_login_ip: model.last_login_ip,
            created_at: model.created_at,
            updated_at: model.updated_at,
            is_active: model.is_active,
            is_staff: model.is_staff,
            is_superuser: model.is_superuser,
        }
    }
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/settings", get(page_settings))
        .route(
            "/settings/departments",
            post(query_departments).put(create_department),
        )
        .route(
            "/settings/departments/{id}",
            get(get_department)
                .patch(update_department)
                .delete(delete_department),
        )
        .route("/settings/users", post(query_users).put(create_user))
        .route(
            "/settings/users/{id}",
            get(get_user).patch(update_user).delete(delete_user),
        )
}

pub async fn page_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let settings = build_settings_payload(&state).await;

    state.render(
        "console/settings.html",
        json!({
            "nav_active": "settings",
            "settings": settings,
            "settings_data": settings,
        }),
    )
}

async fn build_settings_payload(state: &ConsoleState) -> JsonValue {
    let mut data = serde_json::Map::new();
    let now = Utc::now();
    let ami_endpoint = "/ami/v1";

    let mut platform = json!({});
    let mut proxy = json!({});
    let mut useragent = json!({});
    let mut config_meta = json!({ "key_items": [] });
    let mut acl = json!({
        "active_rules": [],
        "embedded_count": 0usize,
        "file_patterns": [],
        "reload_supported": false,
        "metrics": JsonValue::Null,
    });
    let mut operations: Vec<JsonValue> = Vec::new();

    let mut proxy_stats_value = JsonValue::Null;
    let mut useragent_stats_value = JsonValue::Null;

    if let Some(app_state) = state.app_state() {
        let config_arc = app_state.config.clone();
        let config = config_arc.as_ref();

        let uptime_duration = now - app_state.uptime;
        let uptime_seconds = uptime_duration.num_seconds().max(0);
        platform = json!({
            "version": crate::version::get_short_version(),
            "uptime_seconds": uptime_seconds,
            "uptime_pretty": human_duration(uptime_duration),
            "http_addr": config.http_addr.clone(),
            "log_level": config.log_level.clone(),
            "log_file": config.log_file.clone(),
            "config_loaded_at": app_state.config_loaded_at.to_rfc3339(),
            "config_path": app_state.config_path.clone(),
            "generated_at": now.to_rfc3339(),
        });

        let mut key_items: Vec<JsonValue> = Vec::new();
        key_items.push(json!({ "label": "HTTP address", "value": config.http_addr.clone() }));
        if let Some(ext) = config.external_ip.as_ref() {
            key_items.push(json!({ "label": "External IP", "value": ext }));
        }
        if let (Some(start), Some(end)) = (config.rtp_start_port, config.rtp_end_port) {
            key_items.push(json!({ "label": "RTP ports", "value": format!("{}-{}", start, end) }));
        }
        key_items.push(json!({ "label": "Recorder path", "value": config.recorder_path.clone() }));
        key_items
            .push(json!({ "label": "Media cache path", "value": config.media_cache_path.clone() }));
        key_items
            .push(json!({ "label": "Database", "value": mask_database_url(&config.database_url) }));
        if let Some(ref token) = config.restsend_token {
            key_items.push(json!({ "label": "RestSend token", "value": mask_basic(token) }));
        }
        if let Some(ref proxy_url) = config.llmproxy {
            key_items.push(json!({ "label": "LLM proxy", "value": mask_basic(proxy_url) }));
        }
        if let Some(ref console_cfg) = config.console {
            key_items.push(
                json!({ "label": "Console base path", "value": console_cfg.base_path.clone() }),
            );
        }
        if let Some(ref ami_cfg) = config.ami {
            let allows = ami_cfg
                .allows
                .as_ref()
                .map(|items| items.join(", "))
                .unwrap_or_else(|| "127.0.0.1, ::1".to_string());
            key_items.push(json!({ "label": "AMI allow list", "value": allows }));
        }
        if let Some(ref servers) = config.ice_servers {
            key_items.push(
                json!({ "label": "ICE servers", "value": format!("{} entries", servers.len()) }),
            );
        }
        key_items.push(
            json!({ "label": "Config loaded", "value": app_state.config_loaded_at.to_rfc3339() }),
        );
        if let Some(ref path) = app_state.config_path {
            key_items.push(json!({ "label": "Config path", "value": path.clone() }));
        }
        if let Some(summary) = summarize_callrecord(config.callrecord.as_ref()) {
            key_items.push(summary);
        }
        config_meta = json!({ "key_items": key_items });

        if let Some(server) = app_state.sip_server.as_ref() {
            let stats = server.inner.endpoint.inner.get_stats();
            proxy_stats_value = json!({
                "transactions": {
                    "running": stats.running_transactions,
                    "finished": stats.finished_transactions,
                    "waiting_ack": stats.waiting_ack,
                },
                "dialogs": server.inner.dialog_layer.len(),
            });
        }

        if let Some(ua) = app_state.useragent.as_ref() {
            let stats = ua.endpoint.inner.get_stats();
            useragent_stats_value = json!({
                "transactions": {
                    "running": stats.running_transactions,
                    "finished": stats.finished_transactions,
                    "waiting_ack": stats.waiting_ack,
                },
                "dialogs": ua.dialog_layer.len(),
            });
        }

        if let Some(proxy_cfg) = config.proxy.as_ref() {
            proxy = json!({
                "enabled": app_state.sip_server.is_some(),
                "addr": proxy_cfg.addr.clone(),
                "ports": build_port_list(proxy_cfg),
                "modules": proxy_cfg.modules.clone().unwrap_or_default(),
                "max_concurrency": proxy_cfg.max_concurrency,
                "registrar_expires": proxy_cfg.registrar_expires,
                "callid_suffix": proxy_cfg.callid_suffix.clone(),
                "useragent": proxy_cfg.useragent.clone(),
                "ua_whitelist": proxy_cfg.ua_white_list.clone().unwrap_or_default(),
                "ua_blacklist": proxy_cfg.ua_black_list.clone().unwrap_or_default(),
                "data_sources": json!({
                    "routes": format_proxy_data_source(proxy_cfg.routes_source),
                    "trunks": format_proxy_data_source(proxy_cfg.trunks_source),
                }),
                "rtp": proxy_cfg.rtp_config.clone(),
                "user_backends": proxy_cfg
                    .user_backends
                    .iter()
                    .map(backend_kind)
                    .collect::<Vec<_>>(),
                "realms": proxy_cfg.realms.clone().unwrap_or_default(),
                "stats": proxy_stats_value.clone(),
            });
        }

        if let Some(ua_cfg) = config.ua.as_ref() {
            let register_count = ua_cfg
                .register_users
                .as_ref()
                .map(|list| list.len())
                .unwrap_or(0);
            useragent = json!({
                "enabled": app_state.useragent.is_some(),
                "addr": ua_cfg.addr.clone(),
                "udp_port": ua_cfg.udp_port,
                "useragent": ua_cfg.useragent.clone(),
                "callid_suffix": ua_cfg.callid_suffix.clone(),
                "register_users": register_count,
                "accept_timeout": ua_cfg.accept_timeout.clone(),
                "graceful_shutdown": ua_cfg.graceful_shutdown.unwrap_or(true),
                "stats": useragent_stats_value.clone(),
            });
        }

        let (active_rules, embedded_count) = resolve_acl_rules(app_state.clone()).await;
        let acl_files = config
            .proxy
            .as_ref()
            .map(|cfg| cfg.acl_files.clone())
            .unwrap_or_default();
        acl = json!({
            "active_rules": active_rules,
            "embedded_count": embedded_count,
            "file_patterns": acl_files,
            "reload_supported": app_state.sip_server.is_some(),
            "metrics": JsonValue::Null,
        });

        operations.push(json!({
            "id": "reload-acl",
            "label": "Reload ACL rules",
            "description": "Re-read ACL definitions from config files and embedded lists.",
            "method": "POST",
            "endpoint": format!("{}/reload/acl", ami_endpoint.trim_end_matches('/')),
        }));

        let (storage_meta, storage_profiles) = build_storage_profiles(config);

        data.insert("storage".to_string(), storage_meta.clone());
        data.insert(
            "storage_profiles".to_string(),
            JsonValue::Array(storage_profiles.clone()),
        );

        data.insert(
            "server".to_string(),
            json!({
                "operations": operations.clone(),
                "storage": storage_meta,
                "storage_profiles": storage_profiles,
            }),
        );
    } else {
        data.insert("storage".to_string(), json!({ "mode": "unknown" }));
        data.insert(
            "storage_profiles".to_string(),
            JsonValue::Array(Vec::<JsonValue>::new()),
        );
        data.insert(
            "server".to_string(),
            json!({
                "operations": operations.clone(),
                "storage": {"mode": "unknown"},
                "storage_profiles": Vec::<JsonValue>::new(),
            }),
        );
    }

    let stats = json!({
        "generated_at": now.to_rfc3339(),
        "proxy": proxy_stats_value,
        "useragent": useragent_stats_value,
    });

    data.insert("platform".to_string(), platform);
    data.insert("proxy".to_string(), proxy);
    data.insert("useragent".to_string(), useragent);
    data.insert("config".to_string(), config_meta);
    data.insert("acl".to_string(), acl);
    data.insert("stats".to_string(), stats);
    data.insert("ami_endpoint".to_string(), json!(ami_endpoint));
    data.insert(
        "operations".to_string(),
        JsonValue::Array(operations.clone()),
    );

    JsonValue::Object(data)
}

async fn resolve_acl_rules(app_state: Arc<AppStateInner>) -> (Vec<String>, usize) {
    if let Some(server) = app_state.sip_server.as_ref() {
        let (context, embedded) = {
            let embedded = server
                .inner
                .proxy_config
                .acl_rules
                .as_ref()
                .map(|rules| rules.len())
                .unwrap_or(0);
            (server.inner.data_context.clone(), embedded)
        };
        let snapshot = context.acl_rules_snapshot().await;
        (snapshot, embedded)
    } else if let Some(proxy_cfg) = app_state.config.proxy.as_ref() {
        let rules = proxy_cfg.acl_rules.clone().unwrap_or_default();
        let embedded = rules.len();
        (rules, embedded)
    } else {
        (Vec::new(), 0)
    }
}

fn build_storage_profiles(config: &crate::config::Config) -> (JsonValue, Vec<JsonValue>) {
    use serde_json::Map;

    struct Profile {
        id: String,
        label: &'static str,
        description: String,
        config: Map<String, JsonValue>,
    }

    impl Profile {
        fn new(id: impl Into<String>, label: &'static str, description: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                label,
                description: description.into(),
                config: Map::new(),
            }
        }

        fn insert(&mut self, key: &str, value: JsonValue) {
            self.config.insert(key.to_string(), value);
        }

        fn into_json(self) -> JsonValue {
            let mut object = Map::new();
            object.insert("id".to_string(), json!(self.id));
            object.insert("label".to_string(), json!(self.label));
            object.insert("description".to_string(), json!(self.description));
            object.insert("config".to_string(), JsonValue::Object(self.config));
            JsonValue::Object(object)
        }
    }

    let (mode, callrecord_profile) = match config.callrecord.as_ref() {
        Some(CallRecordConfig::Local { root }) => {
            let mut profile = Profile::new(
                "callrecord-local",
                "Call recordings",
                format!("Storing call detail records on {}", root),
            );
            profile.insert("type", json!("local"));
            profile.insert("root", json!(root));
            ("local".to_string(), profile)
        }
        Some(CallRecordConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            root,
            with_media,
            keep_media_copy,
        }) => {
            let mut profile = Profile::new(
                "callrecord-s3",
                "Call recordings",
                format!("Uploading call detail records to S3 bucket {}", bucket),
            );
            let vendor_value = serde_json::to_value(vendor)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| format!("{:?}", vendor).to_lowercase());
            profile.insert("type", json!("s3"));
            profile.insert("vendor", json!(vendor_value));
            profile.insert("bucket", json!(bucket));
            profile.insert("region", json!(region));
            profile.insert("endpoint", json!(endpoint));
            profile.insert("root", json!(root));
            profile.insert("access_key", json!(mask_basic(access_key)));
            profile.insert("secret_key", json!(mask_basic(secret_key)));
            if let Some(flag) = with_media {
                profile.insert("with_media", json!(flag));
            }
            if let Some(flag) = keep_media_copy {
                profile.insert("keep_media_copy", json!(flag));
            }
            ("s3".to_string(), profile)
        }
        Some(CallRecordConfig::Http {
            url,
            headers,
            with_media,
            keep_media_copy,
        }) => {
            let mut profile = Profile::new(
                "callrecord-http",
                "Call recordings",
                "Streaming call detail records to HTTP endpoint",
            );
            profile.insert("type", json!("http"));
            profile.insert("url", json!(url));
            if let Some(headers) = headers {
                profile.insert("headers", json!(headers));
            }
            if let Some(flag) = with_media {
                profile.insert("with_media", json!(flag));
            }
            if let Some(flag) = keep_media_copy {
                profile.insert("keep_media_copy", json!(flag));
            }
            ("http".to_string(), profile)
        }
        None => {
            let mut profile = Profile::new(
                "callrecord-local",
                "Call recordings",
                format!("Storing call detail records on {}", config.recorder_path),
            );
            profile.insert("type", json!("local"));
            profile.insert("root", json!(&config.recorder_path));
            ("local".to_string(), profile)
        }
    };

    let mut spool_profile = Profile::new(
        "spool-paths",
        "Spool directories",
        "Server-side spool paths for recordings and media cache.",
    );
    spool_profile.insert("recorder_path", json!(&config.recorder_path));
    spool_profile.insert("media_cache_path", json!(&config.media_cache_path));

    let active_profile_id = callrecord_profile.id.clone();
    let active_description = callrecord_profile.description.clone();
    let storage_mode = mode.clone();

    let storage_meta = json!({
        "mode": storage_mode,
        "active_profile": active_profile_id,
        "description": active_description,
        "recorder_path": &config.recorder_path,
        "media_cache_path": &config.media_cache_path,
    });

    let profiles = vec![callrecord_profile.into_json(), spool_profile.into_json()];

    (storage_meta, profiles)
}

async fn query_departments(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<forms::ListQuery<QueryDepartmentFilters>>,
) -> Response {
    let db = state.db();
    let mut selector = DepartmentEntity::find().order_by_asc(DepartmentColumn::Name);
    if let Some(filters) = payload.filters.as_ref() {
        if let Some(keyword) = filters
            .q
            .as_ref()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            let pattern = format!("%{}%", keyword);
            selector = selector.filter(
                Condition::any()
                    .add(DepartmentColumn::Name.like(pattern.clone()))
                    .add(DepartmentColumn::DisplayLabel.like(pattern.clone()))
                    .add(DepartmentColumn::Slug.like(pattern)),
            );
        }
    }

    let paginator = selector.paginate(db, payload.normalize().1);
    let pagination = match forms::paginate(paginator, &payload).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to query departments: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
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

    Json(json!({
        "page": current_page,
        "per_page": per_page,
        "total_items": total_items,
        "total_pages": total_pages,
        "has_prev": has_prev,
        "has_next": has_next,
        "items": items,
    }))
    .into_response()
}

async fn get_department(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    match DepartmentEntity::find_by_id(id).one(state.db()).await {
        Ok(Some(model)) => Json(model).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"message": "Department not found"})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to load department {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn create_department(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<DepartmentPayload>,
) -> Response {
    let name = payload.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Department name is required"})),
        )
            .into_response();
    }

    let now = Utc::now();
    let mut active = DepartmentActiveModel {
        name: Set(name.to_string()),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    active.display_label = Set(normalize_opt_string(payload.display_label));
    active.slug = Set(normalize_opt_string(payload.slug));
    active.description = Set(normalize_opt_string(payload.description));
    active.color = Set(normalize_opt_string(payload.color));
    active.manager_contact = Set(normalize_opt_string(payload.manager_contact));
    active.metadata = Set(payload.metadata);

    match active.insert(state.db()).await {
        Ok(model) => (
            StatusCode::CREATED,
            Json(json!({"status": "ok", "id": model.id})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to create department: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn update_department(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<DepartmentPayload>,
) -> Response {
    let model = match DepartmentEntity::find_by_id(id).one(state.db()).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Department not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load department {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let mut active: DepartmentActiveModel = model.into();
    let name = payload.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Department name is required"})),
        )
            .into_response();
    }
    active.name = Set(name.to_string());
    active.display_label = Set(normalize_opt_string(payload.display_label));
    active.slug = Set(normalize_opt_string(payload.slug));
    active.description = Set(normalize_opt_string(payload.description));
    active.color = Set(normalize_opt_string(payload.color));
    active.manager_contact = Set(normalize_opt_string(payload.manager_contact));
    active.metadata = Set(payload.metadata);
    active.updated_at = Set(Utc::now());

    match active.update(state.db()).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to update department {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn delete_department(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let model = match DepartmentEntity::find_by_id(id).one(state.db()).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "Department not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load department {} for delete: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let active: DepartmentActiveModel = model.into();
    match active.delete(state.db()).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to delete department {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn query_users(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(query): Json<forms::ListQuery<QueryUserFilters>>,
) -> Response {
    let db = state.db();
    let mut selector = UserEntity::find().order_by_asc(UserColumn::Username);
    if let Some(filters) = query.filters.as_ref() {
        if let Some(keyword) = filters
            .q
            .as_ref()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            let pattern = format!("%{}%", keyword);
            selector = selector.filter(
                Condition::any()
                    .add(UserColumn::Email.like(pattern.clone()))
                    .add(UserColumn::Username.like(pattern)),
            );
        }
        if let Some(active_only) = filters.active {
            if active_only {
                selector = selector.filter(UserColumn::IsActive.eq(true));
            }
        }
    }

    let paginator = selector.paginate(db, query.normalize().1);
    let pagination = match forms::paginate(paginator, &query).await {
        Ok(pagination) => pagination,
        Err(err) => {
            warn!("failed to query users: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
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

    let view_items: Vec<UserView> = items.into_iter().map(UserView::from).collect();

    Json(json!({
        "page": current_page,
        "per_page": per_page,
        "total_items": total_items,
        "total_pages": total_pages,
        "has_prev": has_prev,
        "has_next": has_next,
        "items": view_items,
    }))
    .into_response()
}

async fn get_user(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    match UserEntity::find_by_id(id).one(state.db()).await {
        Ok(Some(model)) => Json(UserView::from(model)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"message": "User not found"})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to load user {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn create_user(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<UserPayload>,
) -> Response {
    let email = payload.email.trim();
    let username = payload.username.trim();
    if email.is_empty() || username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Email and username are required"})),
        )
            .into_response();
    }
    let password = match payload
        .password
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        Some(password) => password.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"message": "Password is required"})),
            )
                .into_response();
        }
    };

    if email_exists(state.db(), email, None).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Email already in use"})),
        )
            .into_response();
    }
    if username_exists(state.db(), username, None).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Username already in use"})),
        )
            .into_response();
    }

    let now = Utc::now();
    let hashed = match hash_password(&password) {
        Ok(hash) => hash,
        Err(err) => {
            warn!("failed to hash password: {}", err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "Failed to hash password"})),
            )
                .into_response();
        }
    };

    let mut active: UserActiveModel = Default::default();
    active.email = Set(email.to_lowercase());
    active.username = Set(username.to_string());
    active.password_hash = Set(hashed);
    active.created_at = Set(now);
    active.updated_at = Set(now);
    active.is_active = Set(payload.is_active.unwrap_or(true));
    active.is_staff = Set(payload.is_staff.unwrap_or(false));
    active.is_superuser = Set(payload.is_superuser.unwrap_or(false));

    match active.insert(state.db()).await {
        Ok(model) => (
            StatusCode::CREATED,
            Json(json!({"status": "ok", "id": model.id})),
        )
            .into_response(),
        Err(err) => {
            warn!("failed to create user: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn update_user(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<UserPayload>,
) -> Response {
    let model = match UserEntity::find_by_id(id).one(state.db()).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "User not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load user {} for update: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let email = payload.email.trim();
    let username = payload.username.trim();
    if email.is_empty() || username.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Email and username are required"})),
        )
            .into_response();
    }

    if email_exists(state.db(), email, Some(id)).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Email already in use"})),
        )
            .into_response();
    }
    if username_exists(state.db(), username, Some(id)).await {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Username already in use"})),
        )
            .into_response();
    }

    let mut active: UserActiveModel = model.into();
    active.email = Set(email.to_lowercase());
    active.username = Set(username.to_string());
    active.is_active = Set(payload.is_active.unwrap_or(true));
    active.is_staff = Set(payload.is_staff.unwrap_or(false));
    active.is_superuser = Set(payload.is_superuser.unwrap_or(false));
    active.updated_at = Set(Utc::now());

    if let Some(password) = payload
        .password
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        match hash_password(password) {
            Ok(hash) => active.password_hash = Set(hash),
            Err(err) => {
                warn!("failed to hash password: {}", err);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"message": "Failed to hash password"})),
                )
                    .into_response();
            }
        }
    }

    match active.update(state.db()).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to update user {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn delete_user(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let model = match UserEntity::find_by_id(id).one(state.db()).await {
        Ok(Some(model)) => model,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"message": "User not found"})),
            )
                .into_response();
        }
        Err(err) => {
            warn!("failed to load user {} for delete: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let active: UserActiveModel = model.into();
    match active.delete(state.db()).await {
        Ok(_) => Json(json!({"status": "ok"})).into_response(),
        Err(err) => {
            warn!("failed to delete user {}: {}", id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn email_exists(db: &sea_orm::DatabaseConnection, email: &str, exclude: Option<i64>) -> bool {
    let mut selector = UserEntity::find().filter(UserColumn::Email.eq(email));
    if let Some(id) = exclude {
        selector = selector.filter(UserColumn::Id.ne(id));
    }
    selector.count(db).await.unwrap_or(0) > 0
}

async fn username_exists(
    db: &sea_orm::DatabaseConnection,
    username: &str,
    exclude: Option<i64>,
) -> bool {
    let mut selector = UserEntity::find().filter(UserColumn::Username.eq(username));
    if let Some(id) = exclude {
        selector = selector.filter(UserColumn::Id.ne(id));
    }
    selector.count(db).await.unwrap_or(0) > 0
}

fn normalize_opt_string(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
}

fn human_duration(duration: Duration) -> String {
    let total = duration.num_seconds().max(0);
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 {
        parts.push(format!("{}m", minutes));
    }
    if seconds > 0 && parts.is_empty() {
        parts.push(format!("{}s", seconds));
    }

    if parts.is_empty() {
        "0s".to_string()
    } else {
        parts.join(" ")
    }
}

fn mask_basic(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        return "****".to_string();
    }
    let mut masked = String::new();
    masked.extend(&chars[..2]);
    masked.push_str("****");
    masked.extend(&chars[chars.len() - 2..]);
    masked
}

fn mask_database_url(url: &str) -> String {
    if let Some(scheme_pos) = url.find("://") {
        let auth_start = scheme_pos + 3;
        if let Some(auth_end_rel) = url[auth_start..].find('@') {
            let auth_end = auth_start + auth_end_rel;
            let auth = &url[auth_start..auth_end];
            if let Some(colon_pos) = auth.find(':') {
                let user = &auth[..colon_pos];
                return format!(
                    "{}{}{}",
                    &url[..auth_start],
                    format!("{}:****", user),
                    &url[auth_end..]
                );
            }
        }
    }
    url.to_string()
}

fn summarize_callrecord(config: Option<&CallRecordConfig>) -> Option<JsonValue> {
    match config? {
        CallRecordConfig::Local { root } => Some(json!({
            "label": "Call record storage",
            "value": format!("Local ({})", root),
        })),
        CallRecordConfig::S3 {
            bucket,
            region,
            endpoint,
            ..
        } => Some(json!({
            "label": "Call record storage",
            "value": format!("S3 bucket {} ({})", bucket, region),
            "hint": endpoint,
        })),
        CallRecordConfig::Http { url, .. } => Some(json!({
            "label": "Call record storage",
            "value": format!("HTTP {}", url),
        })),
    }
}

fn build_port_list(proxy_cfg: &ProxyConfig) -> Vec<JsonValue> {
    let mut ports = Vec::new();
    if let Some(port) = proxy_cfg.udp_port {
        ports.push(json!({ "label": "UDP", "value": port }));
    }
    if let Some(port) = proxy_cfg.tcp_port {
        ports.push(json!({ "label": "TCP", "value": port }));
    }
    if let Some(port) = proxy_cfg.tls_port {
        ports.push(json!({ "label": "TLS", "value": port }));
    }
    if let Some(port) = proxy_cfg.ws_port {
        ports.push(json!({ "label": "WS", "value": port }));
    }
    ports
}

fn format_proxy_data_source(source: ProxyDataSource) -> &'static str {
    match source {
        ProxyDataSource::Config => "config",
        ProxyDataSource::Database => "database",
    }
}

fn backend_kind(backend: &UserBackendConfig) -> String {
    match backend {
        UserBackendConfig::Memory { .. } => "memory".to_string(),
        UserBackendConfig::Http { .. } => "http".to_string(),
        UserBackendConfig::Plain { .. } => "plain".to_string(),
        UserBackendConfig::Database { .. } => "database".to_string(),
        UserBackendConfig::Extension { .. } => "extension".to_string(),
    }
}
