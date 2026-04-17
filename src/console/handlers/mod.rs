use crate::console::ConsoleState;
use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use http::StatusCode;
use serde_json::json;
use std::sync::Arc;

pub mod addons;
pub mod call_control;
pub mod call_record;
pub mod dashboard;
pub mod diagnostics;
pub mod extension;
pub mod forms;
pub mod licenses;
pub mod metrics;
pub mod notifications;
pub mod presence;
pub mod routing;
pub mod setting;
pub mod sip_trunk;
pub mod sipflow;
pub mod user;
pub mod utils;

pub fn bad_request(message: impl Into<String>) -> axum::response::Response {
    let text = message.into();
    (StatusCode::BAD_REQUEST, Json(json!({ "message": text }))).into_response()
}

pub fn require_field(
    value: &Option<String>,
    field: &str,
) -> Result<String, axum::response::Response> {
    normalize_optional_string(value).ok_or_else(|| bad_request(format!("{} is required", field)))
}

pub fn normalize_optional_string(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

pub fn router(state: Arc<ConsoleState>) -> Router {
    let base_path = state.base_path().to_string();
    let api_prefix = state.api_prefix().to_string();

    // Page routes (nested under base_path)
    let page_routes = Router::new()
        .merge(user::urls())
        .merge(extension::urls())
        .merge(sip_trunk::urls())
        .merge(setting::urls())
        .merge(routing::urls())
        .merge(call_record::urls())
        .merge(diagnostics::urls())
        .merge(call_control::urls())
        .merge(addons::urls())
        .merge(licenses::urls())
        .merge(sipflow::urls())
        .merge(notifications::urls())
        .merge(metrics::urls());

    // API routes (nested under api_prefix)
    let api_routes = Router::new()
        .route("/pending-reloads", get(pending_reloads_handler))
        .merge(presence::api_urls())
        .merge(notifications::api_urls())
        .merge(metrics::api_urls())
        .merge(addons::api_urls());

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .route(
            &format!("{base_path}/dashboard/data"),
            get(self::dashboard::dashboard_data),
        )
        .nest(&base_path, page_routes)
        .nest(&api_prefix, api_routes)
        .with_state(state)
}

async fn pending_reloads_handler(State(state): State<Arc<ConsoleState>>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let pending = state.pending_reload.load(Ordering::Relaxed);
    Json(json!({ "pending": pending }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ConsoleConfig, console::ConsoleState, models::migration::Migrator};
    use axum::{body::to_bytes, extract::State, http::StatusCode};
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

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

    #[tokio::test]
    async fn pending_reloads_handler_false_initially() {
        let state = setup_state().await;
        let response = pending_reloads_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["pending"], false);
    }

    #[tokio::test]
    async fn pending_reloads_handler_true_after_mark() {
        let state = setup_state().await;
        state.mark_pending_reload();

        let response = pending_reloads_handler(State(state)).await.into_response();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["pending"], true);
    }
}
