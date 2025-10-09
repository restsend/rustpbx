use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

pub async fn dashboard(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/dashboard.html",
        json!({
            "nav_active": "dashboard",
            // Placeholder metrics; wire to real metrics later
            "metrics": {
                "recent10": {
                    "total": 0,
                    "trend": "+0%",
                    "util": 0,
                    "answered": 0,
                    "asr": "—",
                    "ans_util": 0,
                    "acd": "0s",
                    "acd_util": 0,
                    "timeline": [0,0,0,0,0,0,0,0,0,0]
                },
                "today": { "acd": "0s" },
                "active": 0,
                "capacity": 0,
                "active_util": 0
            }
        }),
    )
}
