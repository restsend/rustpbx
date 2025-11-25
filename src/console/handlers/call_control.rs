use crate::callrecord::CallRecordHangupReason;
use crate::console::handlers::bad_request;
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::{ProxyCallStateSnapshot, SessionAction};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;
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

#[derive(Debug, Deserialize)]
pub struct CallCommandPayload {
    action: String,
    callee: Option<String>,
    sdp: Option<String>,
    reason: Option<String>,
    code: Option<u16>,
    initiator: Option<String>,
    queue: Option<String>,
    target: Option<String>,
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

    let Some(handle) = registry.get_handle(&session_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Call not found" })),
        )
            .into_response();
    };

    let action_label = payload.action.trim().to_ascii_lowercase();
    let action = match action_label.as_str() {
        "hangup" => {
            let reason = match payload.reason.as_ref() {
                Some(text) if !text.trim().is_empty() => {
                    match CallRecordHangupReason::from_str(text.trim()) {
                        Ok(reason) => Some(reason),
                        Err(_) => return bad_request("invalid hangup reason"),
                    }
                }
                _ => None,
            };
            SessionAction::Hangup {
                reason,
                code: payload.code,
                initiator: payload.initiator.clone(),
            }
        }
        "answer" => SessionAction::AcceptCall {
            callee: payload.callee.clone(),
            sdp: payload.sdp.clone(),
        },
        "transfer" => {
            let target = payload
                .target
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| bad_request("target is required for transfer"));
            match target {
                Ok(value) => SessionAction::from_transfer_target(value),
                Err(response) => return response,
            }
        }
        "enter_queue" | "queue_enter" => {
            let queue = payload
                .queue
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| bad_request("queue is required"));
            match queue {
                Ok(value) => SessionAction::enter_queue(value),
                Err(response) => return response,
            }
        }
        "exit_queue" | "queue_exit" => SessionAction::ExitQueue,
        other => return bad_request(format!("unsupported action: {}", other)),
    };

    match handle.send_command(action) {
        Ok(_) => Json(json!({ "message": "Command dispatched" })).into_response(),
        Err(err) => (
            StatusCode::CONFLICT,
            Json(json!({ "message": format!("Failed to deliver command: {}", err) })),
        )
            .into_response(),
    }
}

fn snapshot_for(
    registry: &Arc<ActiveProxyCallRegistry>,
    session_id: &str,
) -> Option<ProxyCallStateSnapshot> {
    registry
        .get_handle(session_id)
        .map(|handle| handle.snapshot())
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "message": "SIP server unavailable" })),
    )
        .into_response()
}
