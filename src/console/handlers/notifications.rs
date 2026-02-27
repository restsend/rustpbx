use crate::console::{middleware::AuthRequired, ConsoleState};
use crate::models::system_notification::{
    ActiveModel, Column, Entity, Model,
};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder,
};
use sea_orm::sea_query::Order;
use serde_json::json;
use std::sync::Arc;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/notifications", get(list_notifications))
        .route(
            "/api/notifications/unread-count",
            get(unread_count_handler),
        )
        .route("/api/notifications/{id}/read", post(mark_read_handler))
        .route("/api/notifications/read-all", post(mark_all_read_handler))
}

/// Console page: list all system notifications.
pub async fn list_notifications(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let notifications = Entity::find()
        .order_by(Column::CreatedAt, Order::Desc)
        .all(state.db())
        .await
        .unwrap_or_default();

    state.render(
        "console/notifications.html",
        json!({
            "nav_active": "notifications",
            "notifications": notifications,
        }),
    )
}

/// API: return count of unread notifications (used by the sidebar bell badge).
pub async fn unread_count_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> impl IntoResponse {
    let count = Entity::find()
        .filter(Column::Read.eq(false))
        .count(state.db())
        .await
        .unwrap_or(0);
    Json(json!({ "count": count }))
}

/// API: mark a single notification as read.
pub async fn mark_read_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    AxumPath(id): AxumPath<i64>,
) -> impl IntoResponse {
    if let Ok(Some(m)) = Entity::find_by_id(id).one(state.db()).await {
        let mut am: ActiveModel = m.into();
        am.read = Set(true);
        let _ = am.update(state.db()).await;
    }
    Json(json!({ "ok": true }))
}

/// API: mark all notifications as read.
pub async fn mark_all_read_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> impl IntoResponse {
    let unread: Vec<Model> = Entity::find()
        .filter(Column::Read.eq(false))
        .all(state.db())
        .await
        .unwrap_or_default();

    for m in unread {
        let mut am: ActiveModel = m.into();
        am.read = Set(true);
        let _ = am.update(state.db()).await;
    }
    Json(json!({ "ok": true }))
}
