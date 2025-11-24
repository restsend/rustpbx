use crate::callrecord::CallRecordHangupReason;
use crate::console::handlers::bad_request;
use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallRegistry};
use crate::proxy::proxy_call::{ProxyCallCommand, ProxyCallStateSnapshot};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Serialize)]
struct ActiveCallMeta {
    session_id: String,
    caller: Option<String>,
    callee: Option<String>,
    direction: String,
    started_at: DateTime<Utc>,
    answered_at: Option<DateTime<Utc>>,
    status: String,
}

impl From<ActiveProxyCallEntry> for ActiveCallMeta {
    fn from(entry: ActiveProxyCallEntry) -> Self {
        let status = entry.status_label().to_string();
        Self {
            session_id: entry.session_id,
            caller: entry.caller,
            callee: entry.callee,
            direction: entry.direction,
            started_at: entry.started_at,
            answered_at: entry.answered_at,
            status,
        }
    }
}

#[derive(Debug, Serialize)]
struct ProxyCallStateDto {
    session_id: String,
    phase: String,
    started_at: DateTime<Utc>,
    ring_time: Option<DateTime<Utc>>,
    answer_time: Option<DateTime<Utc>>,
    hangup_reason: Option<String>,
    last_error_code: Option<u16>,
    last_error_reason: Option<String>,
    caller: Option<String>,
    callee: Option<String>,
    current_target: Option<String>,
    queue_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct ActiveCallSummaryDto {
    meta: ActiveCallMeta,
    state: Option<ProxyCallStateDto>,
}

#[derive(Debug, Serialize)]
struct ActiveCallDetailDto {
    meta: Option<ActiveCallMeta>,
    state: ProxyCallStateDto,
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
            ActiveCallSummaryDto {
                meta: entry.into(),
                state: snapshot_for(&registry, &session_id),
            }
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

    let detail = ActiveCallDetailDto {
        meta: registry.get(&session_id).map(ActiveCallMeta::from),
        state: snapshot_to_dto(handle.snapshot()),
    };

    Json(json!({ "data": detail })).into_response()
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

    let command = match command_from_payload(&payload) {
        Ok(cmd) => cmd,
        Err(response) => return response,
    };

    match handle.send_command(command) {
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
) -> Option<ProxyCallStateDto> {
    registry
        .get_handle(session_id)
        .map(|handle| snapshot_to_dto(handle.snapshot()))
}

fn snapshot_to_dto(snapshot: ProxyCallStateSnapshot) -> ProxyCallStateDto {
    ProxyCallStateDto {
        session_id: snapshot.session_id,
        phase: snapshot.phase.label().to_string(),
        started_at: snapshot.started_at,
        ring_time: snapshot.ring_time,
        answer_time: snapshot.answer_time,
        hangup_reason: snapshot.hangup_reason.map(|reason| reason.to_string()),
        last_error_code: snapshot.last_error_code,
        last_error_reason: snapshot.last_error_reason,
        caller: snapshot.caller,
        callee: snapshot.callee,
        current_target: snapshot.current_target,
        queue_name: snapshot.queue_name,
    }
}

fn command_from_payload(payload: &CallCommandPayload) -> Result<ProxyCallCommand, Response> {
    let action = payload.action.trim().to_ascii_lowercase();
    match action.as_str() {
        "hangup" => {
            let reason = match payload.reason.as_ref() {
                Some(text) if !text.trim().is_empty() => {
                    match CallRecordHangupReason::from_str(text.trim()) {
                        Ok(reason) => Some(reason),
                        Err(_) => {
                            return Err(bad_request("invalid hangup reason"));
                        }
                    }
                }
                _ => None,
            };
            Ok(ProxyCallCommand::Hangup {
                reason,
                code: payload.code,
                initiator: payload.initiator.clone(),
            })
        }
        "answer" => Ok(ProxyCallCommand::Answer {
            callee: payload.callee.clone(),
            sdp: payload.sdp.clone(),
        }),
        "transfer" => {
            let target = payload
                .target
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| bad_request("target is required for transfer"))?;
            Ok(ProxyCallCommand::Transfer {
                target: target.to_string(),
            })
        }
        "enter_queue" | "queue_enter" => {
            let queue = payload
                .queue
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| bad_request("queue is required"))?;
            Ok(ProxyCallCommand::EnterQueue {
                name: queue.to_string(),
            })
        }
        "exit_queue" | "queue_exit" => Ok(ProxyCallCommand::ExitQueue),
        other => Err(bad_request(format!("unsupported action: {}", other))),
    }
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "message": "SIP server unavailable" })),
    )
        .into_response()
}
