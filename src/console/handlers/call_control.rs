use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::sip_session::SessionSnapshot;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

const DEFAULT_ACTIVE_CALL_LIMIT: usize = 50;
const MAX_ACTIVE_CALL_LIMIT: usize = 500;

pub fn urls() -> Router<Arc<ConsoleState>> {
    Router::new()
        .route("/calls/active", get(list_active_calls))
        .route("/calls/active/{session_id}", get(show_active_call))
        .route(
            "/calls/active/{session_id}/commands",
            post(dispatch_call_command),
        )
}

#[derive(Default, Deserialize)]
pub struct ActiveCallListQuery {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CallCommandPayload {
    Hangup {
        reason: Option<String>,
        code: Option<u16>,
        initiator: Option<String>,
    },
    #[serde(alias = "accept")]
    Accept {
        callee: Option<String>,
        sdp: Option<String>,
    },
    Transfer {
        target: String,
    },
    Mute {
        track_id: String,
    },
    Unmute {
        track_id: String,
    },
}

pub async fn list_active_calls(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Query(query): Query<ActiveCallListQuery>,
) -> Response {
    let Some(server) = state.sip_server() else {
        return service_unavailable();
    };

    let limit = query
        .limit
        .unwrap_or(DEFAULT_ACTIVE_CALL_LIMIT)
        .clamp(1, MAX_ACTIVE_CALL_LIMIT);
    let registry = server.active_call_registry.clone();
    let entries = registry.list_recent(limit);
    let payload: Vec<_> = entries
        .into_iter()
        .map(|entry| {
            let session_id = entry.session_id.clone();
            json!({
                "meta": entry,
                "state": snapshot_for(&registry, &session_id),
            })
        })
        .collect();

    Json(json!({ "data": payload })).into_response()
}

pub async fn show_active_call(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    AxumPath(session_id): AxumPath<String>,
) -> Response {
    let Some(server) = state.sip_server() else {
        return service_unavailable();
    };
    let registry = server.active_call_registry.clone();

    let Some(handle) = registry.get_handle(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Call not found" })),
        )
            .into_response();
    };

    Json(json!({ "data": json!({
        "meta": registry.get(&session_id),
        "state": handle.snapshot(),
    }) }))
    .into_response()
}

pub async fn dispatch_call_command(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    AxumPath(session_id): AxumPath<String>,
    Json(payload): Json<CallCommandPayload>,
) -> Response {
    let Some(server) = state.sip_server() else {
        return service_unavailable();
    };
    let registry = server.active_call_registry.clone();

    // Verify session exists
    let Some(_handle) = registry.get_handle(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Call not found" })),
        )
            .into_response();
    };

    // Use unified dispatch path
    use crate::call::runtime::dispatch_console_command;

    match dispatch_console_command(&registry, &session_id, payload) {
        Ok(result) => {
            if result.success {
                Json(json!({ "message": "Command dispatched" })).into_response()
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": result.message })),
                )
                    .into_response()
            }
        }
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "message": format!("Failed to deliver command: {}", e) })),
        )
            .into_response(),
    }
}

fn snapshot_for(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
) -> Option<SessionSnapshot> {
    registry
        .get_handle(session_id)
        .and_then(|handle| handle.snapshot())
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "message": "SIP server unavailable" })),
    )
        .into_response()
}
