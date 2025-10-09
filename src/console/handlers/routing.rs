use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

pub async fn page_routing(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/routing.html",
        json!({
            "nav_active": "routing",
        }),
    )
}
