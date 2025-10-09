use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

pub async fn page_sip(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/sip_trunk.html",
        json!({
            "nav_active": "sip",
        }),
    )
}
