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
use chrono::{DateTime, Utc};
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
    state.render(
        "console/settings.html",
        json!({
            "nav_active": "settings",
            "settings_data": {
                "server": {
                    "cluster_name": "RustPBX Prod",
                    "external_fqdn": "voice.rustpbx.example",
                    "bind_addresses": ["0.0.0.0:5060", "0.0.0.0:5061", "[::]:5060"],
                    "media_bind": "10.0.12.0/24",
                    "storage": {
                        "mode": "hybrid",
                        "local_path": "/var/lib/rustpbx/media",
                        "s3": {
                            "enabled": true,
                            "bucket": "rustpbx-prod-recordings",
                            "region": "us-west-2",
                            "prefix": "voice/",
                            "access_key": "AKIA••••",
                            "endpoint": "https://s3.us-west-2.amazonaws.com"
                        }
                    },
                    "storage_profiles": [
                        {
                            "id": "local",
                            "label": "Local filesystem",
                            "description": "Store recordings and diagnostics on the PBX host with nightly rsync to cold storage.",
                            "config": {
                                "path": "/var/lib/rustpbx/media",
                                "filesystem": "ext4",
                                "capacity_gb": 512
                            }
                        },
                        {
                            "id": "s3",
                            "label": "Amazon S3 compatible",
                            "description": "Offload assets to an S3 bucket with automatic lifecycle rules.",
                            "config": {
                                "bucket": "rustpbx-prod-recordings",
                                "region": "us-west-2",
                                "prefix": "voice/",
                                "endpoint": "https://s3.us-west-2.amazonaws.com"
                            }
                        }
                    ],
                    "operations": [
                        {"label": "Reload routing", "id": "reload-routing"},
                        {"label": "Reload ACL", "id": "reload-acl"},
                    ],
                    "database": {
                        "provider": "PostgreSQL",
                        "dsn": "postgres://rustpbx:***@pg-cluster.internal:5432/pbx",
                        "pool_size": 32,
                        "replica": "pg-replica.internal:5432"
                    },
                    "last_reload": "2025-10-09T19:12:00Z",
                    "release_channel": "stable"
                },
                "retention": {
                    "call_records_days": 365,
                    "recordings_days": 180,
                    "cdr_export": {
                        "enabled": true,
                        "frequency": "daily",
                        "target": "s3://rustpbx-analytics/cdr/"
                    },
                    "anonymise_after_days": 30
                },
                "security": {
                    "trusted_ips": ["203.0.113.10", "198.51.100.42", "10.0.0.0/16"],
                    "blocked_user_agents": ["friendly-scanner", "sundayddr", "sipcli"],
                    "rate_limits": {
                        "register_per_minute": 30,
                        "options_per_minute": 120,
                        "invite_per_minute": 60
                    },
                    "threat_feed": [
                        {"source": "AbuseIPDB", "last_sync": "2025-10-09T23:00:00Z", "entries": 1243},
                        {"source": "Spamhaus DROP", "last_sync": "2025-10-09T19:10:00Z", "entries": 842}
                    ],
                    "audit": {
                        "last_failed_login": "2025-10-09T17:44:00Z",
                        "policy_version": "2025.09"
                    }
                },
                "asr": {
                    "providers": [
                        {"id": "openai-whisper", "name": "OpenAI Whisper", "token_hint": "sk-live-••••"},
                        {"id": "google-speech", "name": "Google Speech-to-Text", "token_hint": "service-account.json"},
                        {"id": "qcloud-asr", "name": "Tencent Cloud ASR", "token_hint": "secretId/secretKey"}
                    ],
                    "active_provider": "openai-whisper",
                    "callback": {
                        "url": "https://voice.rustpbx.example/hooks/asr",
                        "auth_header": "Bearer ***"
                    },
                    "languages": ["en-US", "zh-CN", "ja-JP"],
                }
            }
        }),
    )
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
