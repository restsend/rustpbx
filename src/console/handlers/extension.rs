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
use axum::routing::get;
use axum::{Json, Router};
use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, Query as SeaQuery};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, ModelTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone, Deserialize, Default)]
struct QueryExtensionsFilters {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    department_ids: Option<Vec<i64>>,
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

    let mut selector = ExtensionEntity::find().order_by_asc(ExtensionColumn::CreatedAt);
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
    }
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

    for ext in &pagination.items {
        let departments = ext
            .find_related(crate::models::department::Entity)
            .all(db)
            .await
            .ok();
        items.push(json!({
            "extension": ext,
            "departments": departments,
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
            email: "tester@example.com".into(),
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
        ConsoleState::initialize(db, ConsoleConfig::default())
            .await
            .expect("initialize console state")
    }

    async fn insert_extension(db: &sea_orm::DatabaseConnection, extension: &str) -> ExtensionModel {
        ExtensionActiveModel {
            extension: Set(extension.to_string()),
            login_disabled: Set(false),
            voicemail_disabled: Set(false),
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
