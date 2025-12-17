use crate::{
    app::AppState,
    config::{Config, ProxyConfig},
    handler::middleware::clientaddr::ClientAddr,
    preflight,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;
use std::sync::{Arc, atomic::Ordering};
use tokio::time::{Duration, sleep};
use tracing::{info, warn};

pub fn router(app_state: AppState) -> Router<AppState> {
    Router::new()
        .route("/lists", get(list_calls))
        .route("/dialogs", get(list_dialogs))
        .route("/transactions", get(list_transactions))
        .route("/kill/{id}", post(kill_call))
        .route("/shutdown", post(shutdown_handler))
        .route("/reload/trunks", post(reload_trunks_handler))
        .route("/reload/routes", post(reload_routes_handler))
        .route("/reload/acl", post(reload_acl_handler))
        .route("/reload/app", post(reload_app_handler))
        .route(
            "/frequency_limits",
            get(list_frequency_limits).delete(clear_frequency_limits),
        )
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            crate::handler::middleware::ami_auth::ami_auth_middleware,
        ))
}

pub(super) async fn health_handler(State(state): State<AppState>) -> Response {
    let ua_stats = match state.useragent {
        Some(ref ua) => {
            let pending_dialogs = ua
                .invitation
                .pending_dialogs
                .lock()
                .map(|ps| ps.len())
                .unwrap_or(0);

            let tx_stats = ua.endpoint.inner.get_stats();
            serde_json::json!({
                "transactions": serde_json::json!({
                    "running": tx_stats.running_transactions,
                    "finished": tx_stats.finished_transactions,
                    "waiting_ack": tx_stats.waiting_ack,
                }),
                "pending": pending_dialogs,
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
        "runnings": state.active_calls.lock().unwrap().len(),
    });
    Json(health).into_response()
}

async fn shutdown_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    warn!(%client_ip, "Shutdown initiated via /shutdown endpoint");
    state.token.cancel();
    Json(serde_json::json!({"status": "shutdown initiated"})).into_response()
}

async fn list_calls(State(state): State<AppState>) -> Response {
    let active_calls = state.active_calls.lock().unwrap();
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
trait DialogInfo {
    fn to_json(&self, source: &str) -> serde_json::Value;
}

impl DialogInfo for rsipstack::dialog::dialog::Dialog {
    fn to_json(&self, source: &str) -> serde_json::Value {
        let state = match &self {
            rsipstack::dialog::dialog::Dialog::ClientInvite(dlg) => dlg.state(),
            rsipstack::dialog::dialog::Dialog::ServerInvite(dlg) => dlg.state(),
        };
        serde_json::json!({
            "id": self.id().to_string(),
            "from": self.from().to_string(),
            "to": self.to().to_string(),
            "state": state.to_string(),
            "source": source,
        })
    }
}

async fn list_dialogs(State(state): State<AppState>) -> Response {
    let mut result = Vec::new();
    if let Some(ref sip_server) = state.sip_server {
        let ids = sip_server.inner.dialog_layer.all_dialog_ids();
        for id in ids {
            if let Some(dialog) = sip_server.inner.dialog_layer.get_dialog(&id) {
                result.push(dialog.to_json("sipserver"));
            }
        }
    }
    if let Some(ref useragent) = state.useragent {
        let ids = useragent.dialog_layer.all_dialog_ids();
        for id in ids {
            if let Some(dialog) = useragent.dialog_layer.get_dialog(&id) {
                result.push(dialog.to_json("useragent"));
            }
        }
    }
    Json(result).into_response()
}

async fn list_transactions(State(state): State<AppState>) -> Response {
    let mut result = Vec::new();
    if let Some(ref sip_server) = state.sip_server {
        sip_server
            .inner
            .endpoint
            .inner
            .get_running_transactions()
            .map(|ids| result.extend(ids));
    }
    if let Some(ref useragent) = state.useragent {
        useragent
            .endpoint
            .inner
            .get_running_transactions()
            .map(|ids| result.extend(ids));
    }

    let result: Vec<String> = result.iter().map(|key| key.to_string()).collect();
    Json(result).into_response()
}

async fn kill_call(
    State(state): State<AppState>,
    Path(id): Path<String>,
    client_ip: ClientAddr,
) -> Response {
    if let Some(call) = state.active_calls.lock().unwrap().remove(&id) {
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

    let config_override = match load_proxy_config_override(&state) {
        Ok(cfg) => cfg,
        Err(response) => return response,
    };

    match sip_server
        .inner
        .data_context
        .reload_trunks(true, config_override)
        .await
    {
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

    let config_override = match load_proxy_config_override(&state) {
        Ok(cfg) => cfg,
        Err(response) => return response,
    };

    match sip_server
        .inner
        .data_context
        .reload_routes(true, config_override)
        .await
    {
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

    let config_override = match load_proxy_config_override(&state) {
        Ok(cfg) => cfg,
        Err(response) => return response,
    };

    match context.reload_acl_rules(true, config_override) {
        Ok(metrics) => {
            let total = metrics.total;
            let active_rules = context.acl_rules_snapshot();
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

fn load_proxy_config_override(state: &AppState) -> Result<Option<Arc<ProxyConfig>>, Response> {
    let Some(path) = state.config_path.as_ref() else {
        return Ok(None);
    };

    match Config::load(path) {
        Ok(cfg) => Ok(cfg.proxy.map(Arc::new)),
        Err(err) => {
            warn!(path = %path, ?err, "configuration reload failed during parsing");
            Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "status": "invalid",
                    "message": format!("Failed to load configuration: {}", err),
                })),
            )
                .into_response())
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ReloadAppParams {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    check_only: bool,
    #[serde(default)]
    dry_run: bool,
}

async fn reload_app_handler(
    State(state): State<AppState>,
    client_ip: ClientAddr,
    Query(params): Query<ReloadAppParams>,
) -> Response {
    let requested_mode = params.mode.as_deref();
    let check_only = params.check_only
        || params.dry_run
        || matches!(requested_mode, Some(mode) if mode.eq_ignore_ascii_case("check") || mode.eq_ignore_ascii_case("validate"));

    info!(%client_ip, check_only, "Reload application via /reload/app endpoint");

    let Some(config_path) = state.config_path.clone() else {
        warn!(%client_ip, "Reload rejected: configuration path unknown");
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "status": "error",
                "message": "Application was started without a configuration file path; reload is unavailable.",
            })),
        )
            .into_response();
    };

    let proposed = match crate::config::Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            warn!(%client_ip, path = %config_path, error = %err, "Configuration reload failed during parsing");
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "status": "invalid",
                    "errors": [{
                        "field": "config",
                        "message": format!("Failed to load configuration: {}", err),
                    }],
                })),
            )
                .into_response();
        }
    };

    if let Err(preflight_error) = preflight::validate_reload(&state, &proposed).await {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "status": "invalid",
                "errors": preflight_error.issues,
            })),
        )
            .into_response();
    }

    if check_only {
        return Json(serde_json::json!({
            "status": "ok",
            "mode": "check",
            "message": "Configuration validated. Services not restarted.",
        }))
        .into_response();
    }

    state.reload_requested.store(true, Ordering::Relaxed);
    let cancel_token = state.token.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(200)).await;
        cancel_token.cancel();
    });

    Json(serde_json::json!({
        "status": "ok",
        "message": "Configuration validated. Restarting services with updated configuration.",
    }))
    .into_response()
}

#[derive(Deserialize)]
struct FrequencyLimitQuery {
    policy_id: Option<String>,
    scope: Option<String>,
    scope_value: Option<String>,
    limit_type: Option<String>,
}

async fn list_frequency_limits(
    State(state): State<AppState>,
    Query(params): Query<FrequencyLimitQuery>,
) -> Response {
    let Some(sip_server) = state.sip_server.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "sip_server_not_running",
            })),
        )
            .into_response();
    };

    let Some(limiter) = sip_server.inner.frequency_limiter.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "frequency_limiter_not_configured",
            })),
        )
            .into_response();
    };

    match limiter
        .list_limits(
            params.policy_id,
            params.scope,
            params.scope_value,
            params.limit_type,
        )
        .await
    {
        Ok(limits) => Json(limits).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": err.to_string(),
            })),
        )
            .into_response(),
    }
}

async fn clear_frequency_limits(
    State(state): State<AppState>,
    Query(params): Query<FrequencyLimitQuery>,
) -> Response {
    let Some(sip_server) = state.sip_server.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "sip_server_not_running",
            })),
        )
            .into_response();
    };

    let Some(limiter) = sip_server.inner.frequency_limiter.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "status": "unavailable",
                "reason": "frequency_limiter_not_configured",
            })),
        )
            .into_response();
    };

    match limiter
        .clear_limits(
            params.policy_id,
            params.scope,
            params.scope_value,
            params.limit_type,
        )
        .await
    {
        Ok(deleted_count) => Json(serde_json::json!({
            "status": "ok",
            "deleted_count": deleted_count,
        }))
        .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": err.to_string(),
            })),
        )
            .into_response(),
    }
}
