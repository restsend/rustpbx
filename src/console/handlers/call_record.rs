use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

pub async fn page_call_records(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/call_records.html",
        json!({ "nav_active": "call-records",}),
    )
}
