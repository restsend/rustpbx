use crate::{app::AppState, handler::middleware::clientaddr::ClientAddr};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
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
        .route("/dialogs", get(list_dialogs))
        .route("/kill/{id}", post(kill_call))
        .route("/shutdown", post(shutdown_handler))
        .route("/reload/trunks", post(reload_trunks_handler))
        .route("/reload/routes", post(reload_routes_handler))
        .route("/reload/acl", post(reload_acl_handler))
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            crate::handler::middleware::ami_auth::ami_auth_middleware,
        ))
}

pub(super) async fn health_handler(State(state): State<AppState>) -> Response {
    let ua_stats = match state.useragent {
        Some(ref ua) => {
            let tx_stats = ua.endpoint.inner.get_stats();
            serde_json::json!({
                "transactions": serde_json::json!({
                    "running": tx_stats.running_transactions,
                    "finished": tx_stats.finished_transactions,
                    "waiting_ack": tx_stats.waiting_ack,
                }),
                "dialogs": ua.dialog_layer.len()
            })
        }
        None => {
            serde_json::json!({})
        }
    };

    let sipserver_stats = match state.sip_server {
        Some(ref server) => {
            let tx_stats = server.inner.endpoint.inner.get_stats();
            serde_json::json!({
                "transactions": serde_json::json!({
                    "running": tx_stats.running_transactions,
                    "finished": tx_stats.finished_transactions,
                    "waiting_ack": tx_stats.waiting_ack,
                }),
                "dialogs": server.inner.dialog_layer.len()
            })
        }
        None => {
            serde_json::json!({})
        }
    };

    let health = serde_json::json!({
        "status": "running",
        "uptime": state.uptime,
        "version": crate::version::get_version_info(),
        "total": state.total_calls.load(Ordering::Relaxed),
        "failed": state.total_failed_calls.load(Ordering::Relaxed),
        "useragent": ua_stats,
        "sipserver": sipserver_stats,
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

async fn list_dialogs(State(state): State<AppState>) -> Response {
    let mut result = Vec::new();
    if let Some(ref sip_server) = state.sip_server {
        let ids = sip_server.inner.dialog_layer.all_dialog_ids();
        for id in ids {
            if let Some(dialog) = sip_server.inner.dialog_layer.get_dialog(&id) {
                let state = match &dialog {
                    rsipstack::dialog::dialog::Dialog::ClientInvite(dlg) => dlg.state(),
                    rsipstack::dialog::dialog::Dialog::ServerInvite(dlg) => dlg.state(),
                };
                result.push(serde_json::json!({
                    "id": dialog.id().to_string(),
                    "from": dialog.from().to_string(),
                    "to": dialog.to().to_string(),
                    "state": state.to_string(),
                }));
            } else {
                result.push(serde_json::json!({
                    "id": id.to_string(),
                    "error": "Dialog not found",
                }));
            }
        }
    }
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

async fn reload_trunks_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    info!(%client_ip, "Reload SIP trunks via /reload/trunks endpoint");

    let Some(sip_server) = state.sip_server.as_ref() else {
        warn!(%client_ip, "Trunk reload ignored: SIP server not running");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "sip_server_not_running",
            })),
        )
            .into_response();
    };

    match sip_server.inner.data_context.reload_trunks().await {
        Ok(metrics) => {
            let total = metrics.total;
            Json(serde_json::json!({
                "status": "ok",
                "trunks_reloaded": total,
                "metrics": metrics,
            }))
        }
        .into_response(),
        Err(error) => {
            warn!(%client_ip, error = %error, "Trunk reload failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "message": error.to_string(),
                })),
            )
                .into_response()
        }
    }
}

async fn reload_routes_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    info!(%client_ip, "Reload routing rules via /reload/routes endpoint");

    let Some(sip_server) = state.sip_server.as_ref() else {
        warn!(%client_ip, "Route reload ignored: SIP server not running");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "sip_server_not_running",
            })),
        )
            .into_response();
    };

    match sip_server.inner.data_context.reload_routes().await {
        Ok(metrics) => {
            let total = metrics.total;
            Json(serde_json::json!({
                "status": "ok",
                "routes_reloaded": total,
                "metrics": metrics,
            }))
        }
        .into_response(),
        Err(error) => {
            warn!(%client_ip, error = %error, "Route reload failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "message": error.to_string(),
                })),
            )
                .into_response()
        }
    }
}

async fn reload_acl_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    info!(%client_ip, "Reload ACL rules via /reload/acl endpoint");

    let Some(sip_server) = state.sip_server.as_ref() else {
        warn!(%client_ip, "ACL reload ignored: SIP server not running");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "sip_server_not_running",
            })),
        )
            .into_response();
    };

    let context = sip_server.inner.data_context.clone();

    match context.reload_acl_rules().await {
        Ok(metrics) => {
            let total = metrics.total;
            let active_rules = context.acl_rules_snapshot().await;
            Json(serde_json::json!({
                "status": "ok",
                "acl_rules_reloaded": total,
                "metrics": metrics,
                "active_rules": active_rules,
            }))
        }
        .into_response(),
        Err(error) => {
            warn!(%client_ip, error = %error, "ACL reload failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "status": "error",
                    "message": error.to_string(),
                })),
            )
                .into_response()
        }
    }
}
