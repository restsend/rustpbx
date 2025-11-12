use super::utils::{build_sensevoice_download_command, command_exists, model_file_path};
use crate::app::AppStateInner;
use crate::config::{CallRecordConfig, Config, ProxyConfig, UserBackendConfig};
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
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use sea_orm::sea_query::Condition;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::{fs, sync::Arc, time::Duration as StdDuration};
use tokio::time::timeout;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};
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
        .route("/settings/config/platform", patch(update_platform_settings))
        .route("/settings/config/storage", patch(update_storage_settings))
        .route("/settings/config/security", patch(update_security_settings))
        .route(
            "/settings/asr/sensevoice/download",
            post(download_sensevoice_model),
        )
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
    AuthRequired(user): AuthRequired,
) -> Response {
    let settings = build_settings_payload(&state).await;

    state.render(
        "console/settings.html",
        json!({
            "nav_active": "settings",
            "settings": settings,
            "settings_data": settings,
            "username": user.username,
            "email": user.email,
            "current_user": {
                "id": user.id,
                "username": user.username,
                "email": user.email,
                "is_superuser": user.is_superuser,
                "is_staff": user.is_staff,
                "is_active": user.is_active,
            },
            "user_is_superuser": user.is_superuser,
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
    let mut console_meta = JsonValue::Null;

    let mut proxy_stats_value = JsonValue::Null;
    let mut useragent_stats_value = JsonValue::Null;

    if let Some(app_state) = state.app_state() {
        let config_arc = app_state.config.clone();
        let mut loaded_config: Option<Config> = None;

        if let Some(path) = app_state.config_path.as_ref() {
            match Config::load(path) {
                Ok(cfg) => {
                    loaded_config = Some(cfg);
                }
                Err(err) => {
                    warn!(config_path = %path, ?err, "failed to reload config from disk");
                }
            }
        }

        let config = loaded_config.as_ref().unwrap_or(config_arc.as_ref());

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

        let recorder_path = config.recorder_path();
        let recorder_format = config.recorder_format();

        let mut key_items: Vec<JsonValue> = Vec::new();
        key_items.push(json!({ "label": "HTTP address", "value": config.http_addr.clone() }));
        if let Some(ext) = config.external_ip.as_ref() {
            key_items.push(json!({ "label": "External IP", "value": ext }));
        }
        if let (Some(start), Some(end)) = (config.rtp_start_port, config.rtp_end_port) {
            key_items.push(json!({ "label": "RTP ports", "value": format!("{}-{}", start, end) }));
        }
        key_items.push(json!({ "label": "Recorder path", "value": recorder_path.clone() }));
        key_items.push(json!({ "label": "Recorder format", "value": recorder_format.extension() }));
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
                    "routes": "toml",
                    "trunks": "toml",
                }),
                "rtp": config.rtp_config(),
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

        if app_state.config_path.is_some() {
            operations.push(json!({
                "id": "reload-app",
                "label": "Reload application",
                "description": "Validate the configuration file and restart core services.",
                "method": "POST",
                "endpoint": format!("{}/reload/app", ami_endpoint.trim_end_matches('/')),
            }));
        }

        let (storage_meta, storage_profiles) = build_storage_profiles(config);

        let mut sensevoice_meta = JsonValue::Null;
        if let Some(transcript_cfg) = config
            .proxy
            .as_ref()
            .and_then(|cfg| cfg.transcript.as_ref())
        {
            let configured_path = transcript_cfg
                .models_path
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string());
            let environment_path = std::env::var("MODEL_PATH")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            let effective_path = environment_path.clone().or(configured_path.clone());
            let model_path_buf = effective_path.as_ref().map(|base| model_file_path(base));

            let mut model_ready = false;
            if let Some(ref path_buf) = model_path_buf {
                if let Ok(meta) = tokio::fs::metadata(path_buf).await {
                    model_ready = meta.is_file();
                }
            }

            let model_file = model_path_buf
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());

            sensevoice_meta = json!({
                "command": transcript_cfg
                    .command
                    .clone()
                    .unwrap_or_else(|| "sensevoice-cli".to_string()),
                "samplerate": transcript_cfg.samplerate,
                "default_language": transcript_cfg.default_language.clone(),
                "timeout_secs": transcript_cfg.timeout_secs,
                "models_path": configured_path,
                "effective_models_path": effective_path,
                "hf_endpoint": transcript_cfg.hf_endpoint.clone(),
                "model_ready": model_ready,
                "model_file": model_file,
            });
        }

        data.insert("storage".to_string(), storage_meta.clone());
        data.insert(
            "storage_profiles".to_string(),
            JsonValue::Array(storage_profiles.clone()),
        );
        data.insert("sensevoice".to_string(), sensevoice_meta);

        data.insert(
            "server".to_string(),
            json!({
                "operations": operations.clone(),
                "storage": storage_meta,
                "storage_profiles": storage_profiles,
            }),
        );

        console_meta = config
            .console
            .as_ref()
            .map(|cfg| {
                json!({
                    "base_path": cfg.base_path,
                    "allow_registration": cfg.allow_registration,
                })
            })
            .unwrap_or(JsonValue::Null);

        let recording_meta = config
            .recording
            .as_ref()
            .and_then(|policy| serde_json::to_value(policy).ok())
            .unwrap_or(JsonValue::Null);
        data.insert("recording".to_string(), recording_meta);
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
        data.insert("recording".to_string(), JsonValue::Null);
        data.insert("sensevoice".to_string(), JsonValue::Null);
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
    data.insert("console".to_string(), console_meta);

    JsonValue::Object(data)
}

async fn resolve_acl_rules(app_state: Arc<AppStateInner>) -> (Vec<String>, usize) {
    if let Some(server) = app_state.sip_server.as_ref() {
        let context = server.inner.data_context.clone();
        let snapshot = context.acl_rules_snapshot().await;

        let embedded = if let Some(path) = app_state.config_path.as_ref() {
            match Config::load(path) {
                Ok(cfg) => cfg
                    .proxy
                    .as_ref()
                    .and_then(|proxy| proxy.acl_rules.as_ref().map(|rules| rules.len()))
                    .unwrap_or(0),
                Err(err) => {
                    warn!(config_path = %path, ?err, "failed to reload config for acl snapshot");
                    server
                        .inner
                        .proxy_config
                        .acl_rules
                        .as_ref()
                        .map(|rules| rules.len())
                        .unwrap_or(0)
                }
            }
        } else {
            server
                .inner
                .proxy_config
                .acl_rules
                .as_ref()
                .map(|rules| rules.len())
                .unwrap_or(0)
        };

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

    let recorder_path = config.recorder_path();
    let recorder_format = config.recorder_format();

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
                format!("Storing call detail records on {}", recorder_path),
            );
            profile.insert("type", json!("local"));
            profile.insert("root", json!(&recorder_path));
            ("local".to_string(), profile)
        }
    };

    let mut spool_profile = Profile::new(
        "spool-paths",
        "Spool directories",
        "Server-side spool paths for recordings and media cache.",
    );
    spool_profile.insert("recorder_path", json!(&recorder_path));
    spool_profile.insert("recorder_format", json!(recorder_format.extension()));
    spool_profile.insert("media_cache_path", json!(&config.media_cache_path));
    if let Some(policy) = config.recording.as_ref() {
        if let Ok(policy_value) = serde_json::to_value(policy) {
            spool_profile.insert("recording", policy_value);
        }
    }

    let active_profile_id = callrecord_profile.id.clone();
    let active_description = callrecord_profile.description.clone();
    let storage_mode = mode.clone();

    let mut storage_meta = serde_json::Map::new();
    storage_meta.insert("mode".to_string(), json!(storage_mode));
    storage_meta.insert("active_profile".to_string(), json!(active_profile_id));
    storage_meta.insert("description".to_string(), json!(active_description));
    storage_meta.insert("recorder_path".to_string(), json!(&recorder_path));
    storage_meta.insert(
        "recorder_format".to_string(),
        json!(recorder_format.extension()),
    );
    storage_meta.insert(
        "media_cache_path".to_string(),
        json!(&config.media_cache_path),
    );
    storage_meta.insert(
        "recording".to_string(),
        config
            .recording
            .as_ref()
            .and_then(|policy| serde_json::to_value(policy).ok())
            .unwrap_or(JsonValue::Null),
    );

    let profiles = vec![callrecord_profile.into_json(), spool_profile.into_json()];

    (JsonValue::Object(storage_meta), profiles)
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
    AuthRequired(user): AuthRequired,
    Json(query): Json<forms::ListQuery<QueryUserFilters>>,
) -> Response {
    if !user.is_superuser {
        return json_error(StatusCode::FORBIDDEN, "Superuser privileges required");
    }
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
    AuthRequired(user): AuthRequired,
) -> Response {
    if !user.is_superuser {
        return json_error(StatusCode::FORBIDDEN, "Superuser privileges required");
    }
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
    AuthRequired(user): AuthRequired,
    Json(payload): Json<UserPayload>,
) -> Response {
    if !user.is_superuser {
        return json_error(StatusCode::FORBIDDEN, "Superuser privileges required");
    }
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
    AuthRequired(user): AuthRequired,
    Json(payload): Json<UserPayload>,
) -> Response {
    if !user.is_superuser {
        return json_error(StatusCode::FORBIDDEN, "Superuser privileges required");
    }
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
    AuthRequired(user): AuthRequired,
) -> Response {
    if !user.is_superuser {
        return json_error(StatusCode::FORBIDDEN, "Superuser privileges required");
    }
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

fn backend_kind(backend: &UserBackendConfig) -> String {
    match backend {
        UserBackendConfig::Memory { .. } => "memory".to_string(),
        UserBackendConfig::Http { .. } => "http".to_string(),
        UserBackendConfig::Plain { .. } => "plain".to_string(),
        UserBackendConfig::Database { .. } => "database".to_string(),
        UserBackendConfig::Extension { .. } => "extension".to_string(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PlatformSettingsPayload {
    #[serde(default)]
    log_level: Option<Option<String>>,
    #[serde(default)]
    log_file: Option<Option<String>>,
    #[serde(default)]
    external_ip: Option<Option<String>>,
    #[serde(default)]
    rtp_start_port: Option<Option<u16>>,
    #[serde(default)]
    rtp_end_port: Option<Option<u16>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StorageSettingsPayload {
    #[serde(default)]
    recorder_path: Option<Option<String>>,
    #[serde(default)]
    media_cache_path: Option<Option<String>>,
    #[serde(default)]
    recorder_format: Option<Option<String>>,
    #[serde(default)]
    callrecord: Option<Option<CallRecordStoragePayload>>,
    #[serde(default)]
    recording_policy: Option<Option<RecordingPolicyPayload>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum CallRecordStoragePayload {
    Disabled,
    Local {
        #[serde(default)]
        root: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecordingPolicyPayload {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    directions: Option<Vec<String>>,
    #[serde(default)]
    caller_allow: Option<Vec<String>>,
    #[serde(default)]
    caller_deny: Option<Vec<String>>,
    #[serde(default)]
    callee_allow: Option<Vec<String>>,
    #[serde(default)]
    callee_deny: Option<Vec<String>>,
    #[serde(default)]
    auto_start: Option<bool>,
    #[serde(default)]
    filename_pattern: Option<Option<String>>,
    #[serde(default)]
    samplerate: Option<Option<u32>>,
    #[serde(default)]
    ptime: Option<Option<u32>>,
    #[serde(default)]
    path: Option<Option<String>>,
    #[serde(default)]
    format: Option<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SecuritySettingsPayload {
    #[serde(default)]
    acl_rules: Option<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SenseVoiceDownloadPayload {
    #[serde(default)]
    models_path: Option<String>,
    #[serde(default)]
    hf_endpoint: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

pub async fn update_platform_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<PlatformSettingsPayload>,
) -> Response {
    let config_path = match get_config_path(&state) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    let mut modified = false;

    if let Some(level_opt) = payload.log_level {
        if let Some(level) = normalize_opt_string(level_opt) {
            doc["log_level"] = value(level);
        } else {
            doc.remove("log_level");
        }
        modified = true;
    }

    if let Some(file_opt) = payload.log_file {
        if let Some(path) = normalize_opt_string(file_opt) {
            doc["log_file"] = value(path);
        } else {
            doc.remove("log_file");
        }
        modified = true;
    }

    if let Some(ext_opt) = payload.external_ip {
        if let Some(ip) = normalize_opt_string(ext_opt) {
            doc["external_ip"] = value(ip);
        } else {
            doc.remove("external_ip");
        }
        modified = true;
    }

    if let Some(start_opt) = payload.rtp_start_port {
        if let Some(port) = start_opt {
            if port == 0 {
                return json_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "rtp_start_port must be greater than 0",
                );
            }
            doc["rtp_start_port"] = value(i64::from(port));
        } else {
            doc.remove("rtp_start_port");
        }
        modified = true;
    }

    if let Some(end_opt) = payload.rtp_end_port {
        if let Some(port) = end_opt {
            if port == 0 {
                return json_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "rtp_end_port must be greater than 0",
                );
            }
            doc["rtp_end_port"] = value(i64::from(port));
        } else {
            doc.remove("rtp_end_port");
        }
        modified = true;
    }

    let doc_text = doc.to_string();
    let config = match parse_config_from_str(&doc_text) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };

    if let (Some(start), Some(end)) = (config.rtp_start_port, config.rtp_end_port) {
        if start > end {
            return json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "rtp_start_port must be less than or equal to rtp_end_port",
            );
        }
    }

    if modified {
        if let Err(resp) = persist_document(&config_path, doc_text) {
            return resp;
        }
    }

    Json(json!({
        "status": "ok",
        "requires_restart": true,
        "message": "Platform settings saved. Restart RustPBX to apply changes.",
        "platform": {
            "log_level": config.log_level,
            "log_file": config.log_file,
        },
        "rtp": {
            "external_ip": config.external_ip,
            "start_port": config.rtp_start_port,
            "end_port": config.rtp_end_port,
        }
    }))
    .into_response()
}

pub async fn update_storage_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<StorageSettingsPayload>,
) -> Response {
    let config_path = match get_config_path(&state) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    let mut modified = false;

    if let Some(path_opt) = payload.recorder_path {
        {
            let table = ensure_table_mut(&mut doc, "recording");
            if let Some(path) = normalize_opt_string(path_opt) {
                table["path"] = value(path);
            } else {
                table.remove("path");
            }
        }
        doc.remove("recorder_path");
        modified = true;
    }

    if let Some(cache_opt) = payload.media_cache_path {
        if let Some(path) = normalize_opt_string(cache_opt) {
            doc["media_cache_path"] = value(path);
        } else {
            doc.remove("media_cache_path");
        }
        modified = true;
    }

    if let Some(format_opt) = payload.recorder_format {
        {
            let table = ensure_table_mut(&mut doc, "recording");
            match format_opt {
                Some(format_value) => {
                    let normalized = format_value.trim().to_ascii_lowercase();
                    if normalized.is_empty() {
                        table.remove("format");
                    } else if normalized == "wav" || normalized == "ogg" {
                        table["format"] = value(normalized);
                    } else {
                        return json_error(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "recorder_format must be either 'wav' or 'ogg'",
                        );
                    }
                }
                None => {
                    table.remove("format");
                }
            }
        }
        doc.remove("recorder_format");
        modified = true;
    }

    if let Some(callrecord_opt) = payload.callrecord {
        match callrecord_opt {
            Some(CallRecordStoragePayload::Disabled) | None => {
                doc.remove("callrecord");
            }
            Some(CallRecordStoragePayload::Local { root }) => {
                let Some(root_path) = normalize_opt_string(root) else {
                    return json_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "callrecord.local.root is required",
                    );
                };
                let mut table = Table::new();
                table["type"] = value("local");
                table["root"] = value(root_path);
                doc["callrecord"] = Item::Table(table);
            }
        }
        modified = true;
    }

    if let Some(policy_opt) = payload.recording_policy {
        match policy_opt {
            Some(policy_payload) => {
                let mut table = Table::new();
                table["enabled"] = value(policy_payload.enabled.unwrap_or(false));

                if let Some(directions) = policy_payload.directions {
                    if directions.is_empty() {
                        table.remove("directions");
                    } else {
                        set_string_array(&mut table, "directions", directions);
                    }
                }

                if let Some(allow) = policy_payload.caller_allow {
                    if allow.is_empty() {
                        table.remove("caller_allow");
                    } else {
                        set_string_array(&mut table, "caller_allow", allow);
                    }
                }

                if let Some(deny) = policy_payload.caller_deny {
                    if deny.is_empty() {
                        table.remove("caller_deny");
                    } else {
                        set_string_array(&mut table, "caller_deny", deny);
                    }
                }

                if let Some(allow) = policy_payload.callee_allow {
                    if allow.is_empty() {
                        table.remove("callee_allow");
                    } else {
                        set_string_array(&mut table, "callee_allow", allow);
                    }
                }

                if let Some(deny) = policy_payload.callee_deny {
                    if deny.is_empty() {
                        table.remove("callee_deny");
                    } else {
                        set_string_array(&mut table, "callee_deny", deny);
                    }
                }

                if let Some(auto_start) = policy_payload.auto_start {
                    table["auto_start"] = value(auto_start);
                }

                match policy_payload.filename_pattern {
                    Some(Some(pattern)) => {
                        let trimmed = pattern.trim();
                        if trimmed.is_empty() {
                            table.remove("filename_pattern");
                        } else {
                            table["filename_pattern"] = value(trimmed);
                        }
                    }
                    Some(None) => {
                        table.remove("filename_pattern");
                    }
                    None => {}
                }

                match policy_payload.samplerate {
                    Some(Some(rate)) => {
                        table["samplerate"] = value(i64::from(rate));
                    }
                    Some(None) => {
                        table.remove("samplerate");
                    }
                    None => {}
                }

                match policy_payload.ptime {
                    Some(Some(ptime)) => {
                        table["ptime"] = value(i64::from(ptime));
                    }
                    Some(None) => {
                        table.remove("ptime");
                    }
                    None => {}
                }

                if let Some(path_opt) = policy_payload.path {
                    if let Some(path) = normalize_opt_string(path_opt) {
                        table["path"] = value(path);
                    } else {
                        table.remove("path");
                    }
                }

                if let Some(format_opt) = policy_payload.format {
                    match format_opt {
                        Some(format_value) => {
                            let normalized = format_value.trim().to_ascii_lowercase();
                            if normalized.is_empty() {
                                table.remove("format");
                            } else if normalized == "wav" || normalized == "ogg" {
                                table["format"] = value(normalized);
                            } else {
                                return json_error(
                                    StatusCode::UNPROCESSABLE_ENTITY,
                                    "recording.format must be either 'wav' or 'ogg'",
                                );
                            }
                        }
                        None => {
                            table.remove("format");
                        }
                    }
                }

                doc["recording"] = Item::Table(table);
            }
            None => {
                doc.remove("recording");
            }
        }
        modified = true;
    }

    let doc_text = doc.to_string();
    let config = match parse_config_from_str(&doc_text) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };

    if modified {
        if let Err(resp) = persist_document(&config_path, doc_text) {
            return resp;
        }
    }

    let (storage_meta, storage_profiles) = build_storage_profiles(&config);

    Json(json!({
        "status": "ok",
        "requires_restart": true,
        "message": "Storage settings saved. Restart RustPBX to apply changes.",
        "storage": storage_meta,
        "storage_profiles": storage_profiles,
    }))
    .into_response()
}

pub async fn update_security_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<SecuritySettingsPayload>,
) -> Response {
    let config_path = match get_config_path(&state) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    let mut modified = false;

    if let Some(acl_opt) = payload.acl_rules {
        let table = ensure_table_mut(&mut doc, "proxy");
        match acl_opt {
            Some(raw) => {
                let rules = parse_lines_to_vec(&raw);
                if rules.is_empty() {
                    table.remove("acl_rules");
                } else {
                    set_string_array(table, "acl_rules", rules);
                }
            }
            None => {
                table.remove("acl_rules");
            }
        }
        modified = true;
    }

    let doc_text = doc.to_string();
    let config = match parse_config_from_str(&doc_text) {
        Ok(cfg) => cfg,
        Err(resp) => return resp,
    };

    if modified {
        if let Err(resp) = persist_document(&config_path, doc_text) {
            return resp;
        }
    }

    let acl_rules = match config.proxy.as_ref() {
        Some(proxy_cfg) => proxy_cfg.acl_rules.clone().unwrap_or_default(),
        None => Vec::new(),
    };

    if let Some(app_state) = state.app_state() {
        if let Some(server) = app_state.sip_server.as_ref() {
            server
                .inner
                .data_context
                .set_acl_rules(acl_rules.clone())
                .await;
        }
    }

    Json(json!({
        "status": "ok",
        "requires_restart": false,
        "message": "Security settings saved and applied.",
        "security": {
            "acl_rules": acl_rules,
        }
    }))
    .into_response()
}

pub async fn download_sensevoice_model(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Json(payload): Json<SenseVoiceDownloadPayload>,
) -> Response {
    let Some(app_state) = state.app_state() else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Application state is unavailable.",
        );
    };

    let Some(config_path) = app_state.config_path.clone() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Configuration file path is unknown. Start the service with --conf to enable editing.",
        );
    };

    let Some(proxy_cfg) = app_state.config.proxy.as_ref() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "Proxy configuration is unavailable.",
        );
    };

    let Some(transcript_cfg) = proxy_cfg.transcript.as_ref() else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "SenseVoice CLI transcription is not configured.",
        );
    };

    let transcript_cfg = transcript_cfg.clone();

    let command = normalize_opt_string(payload.command.clone())
        .or_else(|| transcript_cfg.command.clone())
        .unwrap_or_else(|| "sensevoice-cli".to_string());

    if !command_exists(&command) {
        return json_error(
            StatusCode::FAILED_DEPENDENCY,
            format!(
                "sensevoice-cli is not available (looked for '{}'). Install via `cargo install sensevoice-cli` or configure proxy.transcript.command.",
                command
            ),
        );
    }

    let models_path = normalize_opt_string(payload.models_path.clone())
        .or_else(|| transcript_cfg.models_path.clone())
        .or_else(|| {
            std::env::var("MODEL_PATH")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });

    let models_path = match models_path {
        Some(path) => path,
        None => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "SenseVoice models_path is required. Set MODEL_PATH or proxy.transcript.models_path first.",
            );
        }
    };

    if let Err(err) = tokio::fs::create_dir_all(&models_path).await {
        warn!(path = %models_path, ?err, "failed to prepare models directory");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Failed to prepare models directory '{}': {}",
                models_path, err
            ),
        );
    }

    let hf_endpoint =
        normalize_opt_string(payload.hf_endpoint.clone()).or(transcript_cfg.hf_endpoint.clone());

    let timeout_secs = transcript_cfg.timeout_secs.unwrap_or(600);

    let output_result = if timeout_secs > 0 {
        let mut cmd =
            build_sensevoice_download_command(&command, &models_path, hf_endpoint.as_deref());
        match timeout(StdDuration::from_secs(timeout_secs), cmd.output()).await {
            Ok(result) => result,
            Err(_) => {
                return json_error(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!(
                        "sensevoice-cli download timed out after {} seconds",
                        timeout_secs
                    ),
                );
            }
        }
    } else {
        build_sensevoice_download_command(&command, &models_path, hf_endpoint.as_deref())
            .output()
            .await
    };

    let output = match output_result {
        Ok(output) => output,
        Err(err) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                format!("Failed to execute sensevoice-cli: {}", err),
            );
        }
    };

    if !output.status.success() {
        let stderr_preview = String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(400)
            .collect::<String>();
        return json_error(
            StatusCode::BAD_GATEWAY,
            format!(
                "sensevoice-cli exited with status {}: {}",
                output.status.code().unwrap_or(-1),
                stderr_preview
            ),
        );
    }

    let mut doc = match load_document(&config_path) {
        Ok(doc) => doc,
        Err(resp) => return resp,
    };

    let proxy_table = ensure_table_mut(&mut doc, "proxy");
    if !proxy_table["transcript"].is_table() {
        proxy_table["transcript"] = Item::Table(Table::new());
    }
    let transcript_table = proxy_table["transcript"].as_table_mut().expect("table");
    transcript_table["command"] = value(command.clone());
    transcript_table["models_path"] = value(models_path.clone());
    match hf_endpoint {
        Some(ref endpoint) => {
            transcript_table["hf_endpoint"] = value(endpoint.clone());
        }
        None => {
            transcript_table.remove("hf_endpoint");
        }
    }

    let doc_text = doc.to_string();
    if let Err(resp) = parse_config_from_str(&doc_text) {
        return resp;
    }

    if let Err(resp) = persist_document(&config_path, doc_text) {
        return resp;
    }

    let model_file = model_file_path(&models_path);
    let model_ready = tokio::fs::metadata(&model_file)
        .await
        .map(|meta| meta.is_file())
        .unwrap_or(false);

    let stdout_preview = String::from_utf8_lossy(&output.stdout)
        .trim()
        .chars()
        .take(4000)
        .collect::<String>();
    let stderr_preview = String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .take(4000)
        .collect::<String>();

    Json(json!({
        "status": "ok",
        "message": "SenseVoice model download completed.",
        "models_path": models_path,
        "model_file": model_file.display().to_string(),
        "model_ready": model_ready,
        "stdout": stdout_preview,
        "stderr": stderr_preview,
    }))
    .into_response()
}

fn get_config_path(state: &ConsoleState) -> Result<String, Response> {
    let Some(app_state) = state.app_state() else {
        return Err(json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Application state is unavailable.",
        ));
    };
    let Some(path) = app_state.config_path.clone() else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "Configuration file path is unknown. Start the service with --conf to enable editing.",
        ));
    };
    Ok(path)
}

fn load_document(path: &str) -> Result<DocumentMut, Response> {
    let contents = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read configuration file: {}", err),
            ));
        }
    };

    contents.parse::<DocumentMut>().map_err(|err| {
        json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Configuration file is not valid TOML: {}", err),
        )
    })
}

fn persist_document(path: &str, contents: String) -> Result<(), Response> {
    fs::write(path, contents).map_err(|err| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write configuration file: {}", err),
        )
    })
}

fn parse_config_from_str(contents: &str) -> Result<Config, Response> {
    toml::from_str::<Config>(contents)
        .map(|mut cfg| {
            cfg.ensure_recording_defaults();
            cfg
        })
        .map_err(|err| {
            json_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Configuration validation failed: {}", err),
            )
        })
}

fn ensure_table_mut<'doc>(doc: &'doc mut DocumentMut, key: &str) -> &'doc mut Table {
    if !doc[key].is_table() {
        doc[key] = Item::Table(Table::new());
    }
    doc[key].as_table_mut().expect("table")
}

fn parse_lines_to_vec(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect()
}

fn set_string_array(table: &mut Table, key: &str, values: Vec<String>) {
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    table[key] = Item::Value(Value::Array(array));
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "status": "error",
            "message": message.into(),
        })),
    )
        .into_response()
}
