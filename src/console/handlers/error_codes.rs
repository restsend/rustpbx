//! Live operation-manual page for the standardized call-error registry.
//!
//! Renders the merged [`CallErrRegistry`] at `/console/error-codes` so the
//! operator handbook always stays in sync with the code. The same registry
//! powers call-record error rendering (the `error_code` metadata key is
//! resolved back to a localized message + remediation hint).

use crate::call_errors::CallErrInfo;
use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{Json, Router, extract::{Query, State}, response::{IntoResponse, Response}, routing::get};
use http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
pub struct ErrorCodesQuery {
    pub app: Option<String>,
}

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/error-codes", get(page_error_codes))
        .route("/api/error-codes", get(api_error_codes))
}

async fn error_codes_view() -> JsonValue {
    let reg = crate::call_errors::registry();
    // Group by app for the manual.
    let mut groups: std::collections::BTreeMap<&'static str, Vec<JsonValue>> =
        std::collections::BTreeMap::new();
    for entry in reg.all() {
        groups.entry(entry.app).or_default().push(entry_to_json(entry));
    }
    let grouped: Vec<JsonValue> = groups
        .into_iter()
        .map(|(app, items)| {
            json!({
                "app": app,
                "items": items,
            })
        })
        .collect();
    json!({
        "total": reg.len(),
        "groups": grouped,
    })
}

fn entry_to_json(entry: &CallErrInfo) -> JsonValue {
    json!({
        "app": entry.app,
        "code": entry.code,
        "message": entry.message,
        "sip_status": entry.sip_status,
        "hangup_reason": entry.hangup_reason.to_string(),
        "severity": entry.severity.as_str(),
        "locale_key": entry.locale_key,
        "remediation_key": entry.remediation_key,
    })
}

pub async fn page_error_codes(
    State(state): State<Arc<ConsoleState>>,
    Query(query): Query<ErrorCodesQuery>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let current_user = state.build_current_user_ctx(&user).await;
    // Merge initial_app into the tojson-encoded `data` object (single-quoted
    // x-data attribute, so a user-supplied ?app= value cannot break Alpine's
    // expression — tojson escapes quotes as \uXXXX).
    let mut view = error_codes_view().await;
    view["initial_app"] = json!(query.app.clone().unwrap_or_default());
    state.render_with_headers(
        "console/error_codes.html",
        json!({
            "nav_active": "error_codes",
            "data": view,
            "current_user": current_user,
        }),
        &headers,
    )
}

pub async fn api_error_codes(
    State(_state): State<Arc<ConsoleState>>,
    AuthRequired(_user): AuthRequired,
) -> Response {
    Json(error_codes_view().await).into_response()
}
