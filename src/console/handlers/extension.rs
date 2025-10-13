use crate::console::handlers::forms::{self, ExtensionForm, ListQuery};
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::{
    department::Column as DepartmentColumn,
    department::Entity as DepartmentEntity,
    extension::{
        ActiveModel as ExtensionActiveModel, Column as ExtensionColumn, Entity as ExtensionEntity,
    },
};
use axum::routing::get;
use axum::{Json, Router};
use axum::{
    extract::{Form, Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, EntityTrait, PaginatorTrait, QueryOrder};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, Deserialize, Default)]
struct QueryExtensionsFilters {}

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

    return json!({
        "departments": departments,
    });
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

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "model": model,
            "departments": departments,
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
        }),
    )
}

async fn page_extension_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "filters": build_filters(state.clone()).await,
            "create_url": state.url_for("/extensions/new"),
        }),
    )
}

async fn create_extension(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<ExtensionForm>,
) -> Response {
    let db = state.db();
    let extension = match form.extension {
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
        display_name: Set(form.display_name),
        email: Set(form.email),
        sip_password: Set(form.sip_password),
        call_forwarding_mode: Set(form.call_forwarding_mode),
        call_forwarding_destination: Set(form.call_forwarding_destination),
        call_forwarding_timeout: Set(form.call_forwarding_timeout),
        login_disabled: Set(form.login_disabled.unwrap_or(false)),
        voicemail_disabled: Set(form.voicemail_disabled.unwrap_or(false)),
        notes: Set(form.notes),
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
    let department_ids = form.department_ids.unwrap_or_default();
    if let Err(err) = ExtensionEntity::replace_departments(db, model.id, &department_ids).await {
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
    Json(json!({"status": "ok", "id": model.id})).into_response()
}

async fn query_extensions(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<ListQuery<QueryExtensionsFilters>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let db = state.db();
    let (_, per_page) = query.normalize();
    let filters = build_filters(state.clone()).await;
    let selector = ExtensionEntity::find().order_by_asc(ExtensionColumn::CreatedAt);

    let paginator = selector.paginate(db, per_page);
    let pagination = match forms::paginate(paginator, query).await {
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

    Json(json!({
        "pagination": pagination,
        "filters": filters,
    }))
    .into_response()
}

async fn update_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Form(form): Form<ExtensionForm>,
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
    if let Some(extension) = form.extension {
        active.extension = Set(extension);
    }
    if let Some(display_name) = form.display_name {
        active.display_name = Set(Some(display_name));
    }
    if let Some(email) = form.email {
        active.email = Set(Some(email));
    }
    if let Some(sip_password) = form.sip_password {
        active.sip_password = Set(Some(sip_password));
    }
    if let Some(call_forwarding_mode) = form.call_forwarding_mode {
        active.call_forwarding_mode = Set(Some(call_forwarding_mode));
    }
    if let Some(call_forwarding_destination) = form.call_forwarding_destination {
        active.call_forwarding_destination = Set(Some(call_forwarding_destination));
    }
    if let Some(call_forwarding_timeout) = form.call_forwarding_timeout {
        active.call_forwarding_timeout = Set(Some(call_forwarding_timeout));
    }
    if let Some(notes) = form.notes {
        active.notes = Set(Some(notes));
    }
    if let Some(login_disabled) = form.login_disabled {
        active.login_disabled = Set(login_disabled);
    }
    if let Some(voicemail_disabled) = form.voicemail_disabled {
        active.voicemail_disabled = Set(voicemail_disabled);
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
    let department_ids = form.department_ids.unwrap_or_default();
    if let Err(err) = ExtensionEntity::replace_departments(db, id, &department_ids).await {
        warn!("failed to update departments for extension {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": err.to_string()})),
        )
            .into_response();
    }
    Json(json!({"status": "ok"})).into_response()
}

async fn delete_extension(
    AxumPath(id): AxumPath<i64>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
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
            warn!("failed to load extension {} for delete: {}", id, err);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": err.to_string()})),
            )
                .into_response();
        }
    };

    let active: ExtensionActiveModel = model.into();
    if let Err(err) = active.delete(db).await {
        warn!("failed to delete extension {}: {}", id, err);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message": err.to_string()})),
        )
            .into_response();
    }

    Json(json!({"status": "ok"})).into_response()
}
