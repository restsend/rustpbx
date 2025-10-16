use crate::console::ConsoleState;
use axum::{Json, Router, response::IntoResponse, routing::get};
use http::StatusCode;
use serde_json::json;
use std::sync::Arc;

pub mod bill_template;
pub mod call_record;
pub mod dashboard;
pub mod diagnostics;
pub mod extension;
pub mod forms;
pub mod routing;
pub mod setting;
pub mod sip_trunk;
pub mod user;

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
        .merge(bill_template::urls())
        .merge(sip_trunk::urls())
        .merge(setting::urls())
        .merge(routing::urls())
        .merge(call_record::urls())
        .route("/diagnostics", get(self::diagnostics::page_diagnostics));

    Router::new()
        .route(&format!("{base_path}/"), get(self::dashboard::dashboard))
        .nest(&base_path, routes)
        .with_state(state)
}
