use crate::{app::AppState, handler::middleware::clientaddr::ClientAddr};
use axum::{
    Json, Router,
    extract::{Path, State},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

pub fn router(app_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/lists", get(list_calls))
        .route("/kill/{id}", post(kill_call))
        .route("/shutdown", post(shutdown_handler))
        .route("/reload", post(reload_handler))
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            crate::handler::middleware::ami_auth::ami_auth_middleware,
        ))
}

pub(super) async fn health_handler(State(state): State<AppState>) -> Response {
    let transactions = state
        .useragent
        .as_ref()
        .map(|ua| ua.endpoint.inner.get_stats())
        .map(|stats| {
            serde_json::json!({
                "running": stats.running_transactions,
                "finished": stats.finished_transactions,
                "waiting_ack": stats.waiting_ack,
            })
        });

    let dialogs = state
        .useragent
        .as_ref()
        .map(|ua| ua.dialog_layer.len())
        .unwrap_or_default();

    let health = serde_json::json!({
        "status": "running",
        "uptime": state.uptime,
        "version": crate::version::get_version_info(),
        "total": state.total_calls.load(Ordering::Relaxed),
        "failed": state.total_failed_calls.load(Ordering::Relaxed),
        "ua.transactions": transactions,
        "ua.dialogs": dialogs,
        "runnings": state.active_calls.lock().await.len(),
    });
    Json(health).into_response()
}

async fn shutdown_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    warn!(%client_ip, "Shutdown initiated via /shutdown endpoint");
    state.token.cancel();
    Json(serde_json::json!({"status": "shutdown initiated"})).into_response()
}

async fn list_calls(State(state): State<AppState>) -> Response {
    let active_calls = state.active_calls.lock().await;
    let result = serde_json::json!({
        "total": active_calls.len(),
        "calls": active_calls.iter().map(|(id, call)| {
            let call_state = match call.call_state.read() {
                Ok(call_state) => call_state,
                Err(_) => return serde_json::json!({"id": id, "error": "Failed to read call state"}),
            };
            serde_json::json!({
                "id": id,
                "callType": call.call_type,
                "startTime": call_state.start_time.to_rfc3339(),
                "ringTime": call_state.ring_time.map(|t| t.to_rfc3339()),
                "answerTime": call_state.answer_time.map(|t| t.to_rfc3339()),
                "duration": call_state.answer_time
                    .map(|t| (Utc::now() - t).num_seconds()),
            })
        }).collect::<Vec<_>>(),
    });
    Json(result).into_response()
}

async fn kill_call(
    State(state): State<AppState>,
    Path(id): Path<String>,
    client_ip: ClientAddr,
) -> Response {
    if let Some(call) = state.active_calls.lock().await.remove(&id) {
        call.cancel_token.cancel();
        info!(id, %client_ip, "call killed");
    }
    Json(true).into_response()
}

async fn reload_handler(State(_state): State<AppState>, client_ip: ClientAddr) -> Response {
    info!(%client_ip, "Reload configuration initiated via /reload endpoint");
    Json(serde_json::json!({"status": "configuration reloaded"})).into_response()
}
