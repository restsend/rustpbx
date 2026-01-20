use crate::console::ConsoleState;
use axum::{Json, Router, response::IntoResponse, routing::get};
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
    let routes = Router::new()
        .merge(user::urls())
        .merge(extension::urls())
        .merge(sip_trunk::urls())
        .merge(setting::urls())
        .merge(routing::urls())
        .merge(call_record::urls())
        .merge(diagnostics::urls())
        .merge(call_control::urls())
        .merge(presence::urls())
        .merge(addons::urls())
        .merge(sipflow::urls());

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .route(
            &format!("{base_path}/dashboard/data"),
            get(self::dashboard::dashboard_data),
        )
        .nest(&base_path, routes)
        .with_state(state)
}
