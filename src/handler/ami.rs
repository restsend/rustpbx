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
use serde::Deserialize;
use std::sync::{Arc, atomic::Ordering};
use tokio::time::{Duration, sleep};
use tracing::{info, warn};

pub fn ami_router(app_state: AppState) -> Router<AppState> {
    let r = Router::new()
        .route("/health", get(health_handler))
        .route("/dialogs", get(list_dialogs))
        .route("/hangup/{id}", get(hangup_dialog))
        .route("/transactions", get(list_transactions))
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
        ));
    Router::new().nest("/ami/v1", r).with_state(app_state)
}

pub(super) async fn health_handler(State(state): State<AppState>) -> Response {
    let tx_stats = state.sip_server().inner.endpoint.inner.get_stats();
    let app_tasks = {
        let metrics = crate::utils::GLOBAL_TASK_METRICS.lock().unwrap();
        metrics
            .iter()
            .filter(|&(_, &v)| v > 0)
            .map(|(k, &v)| (k.clone(), serde_json::json!(v)))
            .collect::<serde_json::Map<String, serde_json::Value>>()
    };

    let sipserver_stats = serde_json::json!({
        "transactions": serde_json::json!({
            "running": tx_stats.running_transactions,
            "finished": tx_stats.finished_transactions,
            "waiting_ack": tx_stats.waiting_ack,
        }),
        "dialogs": state.sip_server().inner.dialog_layer.len(),
        "calls": state.sip_server().inner.active_call_registry.count(),
        "running_tx": state.sip_server().inner.runnings_tx.load(Ordering::Relaxed),
    });

    let callrecord_stats = match state.core.callrecord_stats {
        Some(ref stats) => serde_json::json!(stats.as_ref() as &crate::callrecord::CallRecordStats),
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
        "tasks": app_tasks,
        "sipserver": sipserver_stats,
        "callrecord": callrecord_stats,
    });
    Json(health).into_response()
}

async fn shutdown_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    warn!(%client_ip, "Shutdown initiated via /shutdown endpoint");
    state.token().cancel();
    Json(serde_json::json!({"status": "shutdown initiated"})).into_response()
}

trait DialogInfo {
    fn to_json(&self) -> serde_json::Value;
}

impl DialogInfo for rsipstack::dialog::dialog::Dialog {
    fn to_json(&self) -> serde_json::Value {
        let state = self.state();
        serde_json::json!({
            "state": state.to_string(),
            "from": self.from().to_string(),
            "to": self.to().to_string()
        })
    }
}

async fn list_dialogs(State(state): State<AppState>) -> Response {
    let mut result = Vec::new();
    let ids = state.sip_server().inner.dialog_layer.all_dialog_ids();
    for id in ids {
        if let Some(dialog) = state.sip_server().inner.dialog_layer.get_dialog_with(&id) {
            result.push(dialog.to_json());
        }
    }
    Json(result).into_response()
}

async fn hangup_dialog(Path(id): Path<String>, State(state): State<AppState>) -> Response {
    match state.sip_server.inner.dialog_layer.get_dialog_with(&id) {
        Some(dlg) => match dlg.hangup().await {
            Ok(()) => {
                return Json(serde_json::json!({
                    "status": "ok",
                    "message": format!("Dialog with id '{}' hangup initiated", id),
                }))
                .into_response();
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "status": "error",
                        "message": format!("Failed to hangup dialog with id '{}': {}", id, err),
                    })),
                )
                    .into_response();
            }
        },
        None => {}
    }
    return (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "status": "error",
            "message": format!("Dialog with id '{}' not found", id),
        })),
    )
        .into_response();
}

async fn list_transactions(State(state): State<AppState>) -> Response {
    let mut result = Vec::new();
    state
        .sip_server()
        .inner
        .endpoint
        .inner
        .get_running_transactions()
        .map(|ids| result.extend(ids));
    let result: Vec<String> = result.iter().map(|key| key.to_string()).collect();
    Json(result).into_response()
}

async fn reload_trunks_handler(State(state): State<AppState>, client_ip: ClientAddr) -> Response {
    info!(%client_ip, "Reload SIP trunks via /reload/trunks endpoint");

    let config_override = match load_proxy_config_override(&state) {
        Ok(cfg) => cfg,
        Err(response) => return response,
    };

    match state
        .sip_server()
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

    let config_override = match load_proxy_config_override(&state) {
        Ok(cfg) => cfg,
        Err(response) => return response,
    };

    match state
        .sip_server()
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
    let context = state.sip_server().inner.data_context.clone();

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
        Ok(cfg) => Ok(Some(Arc::new(cfg.proxy))),
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
    let cancel_token = state.token().clone();
    crate::utils::spawn(async move {
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
    let Some(limiter) = state.sip_server().inner.frequency_limiter.as_ref() else {
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
    let Some(limiter) = state.sip_server().inner.frequency_limiter.as_ref() else {
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
