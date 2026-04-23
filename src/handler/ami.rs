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
use chrono::TimeZone;
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
        .route("/trunk_registrations", get(trunk_registrations_handler))
        .route("/reload/routes", post(reload_routes_handler))
        .route("/reload/acl", post(reload_acl_handler))
        .route("/reload/app", post(reload_app_handler))
        .route(
            "/frequency_limits",
            get(list_frequency_limits).delete(clear_frequency_limits),
        )
        .route("/sipflow/flow/{call_id}", get(query_sipflow_flow))
        .route("/sipflow/media/{call_id}", get(query_sipflow_media))
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
    if let Some(dlg) = state.sip_server.inner.dialog_layer.get_dialog_with(&id) { match dlg.hangup().await {
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
    } }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "status": "error",
            "message": format!("Dialog with id '{}' not found", id),
        })),
    )
        .into_response()
}

async fn list_transactions(State(state): State<AppState>) -> Response {
    let mut result = Vec::new();
    if let Some(ids) = state
        .sip_server()
        .inner
        .endpoint
        .inner
        .get_running_transactions() { result.extend(ids) }
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
            if let Some(ref console) = state.console {
                console.clear_pending_reload();
            }
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

async fn trunk_registrations_handler(State(state): State<AppState>) -> Response {
    let statuses = state
        .sip_server()
        .inner
        .data_context
        .trunk_registrar()
        .get_statuses();
    Json(serde_json::json!({
        "registrations": statuses,
    }))
    .into_response()
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
            if let Some(ref console) = state.console {
                console.clear_pending_reload();
            }
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

#[allow(clippy::result_large_err)]
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

#[derive(Debug, Deserialize)]
struct SipFlowQueryParams {
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

async fn query_sipflow_flow(
    State(state): State<AppState>,
    Path(call_id): Path<String>,
    Query(params): Query<SipFlowQueryParams>,
) -> Response {
    use crate::models::call_record::{Column as CallRecordColumn, Entity as CallRecordEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let sip_server = state.sip_server();

    let sipflow = match &sip_server.inner.sip_flow {
        Some(flow) => flow,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "SipFlow not enabled"
                })),
            )
                .into_response();
        }
    };

    let backend = match sipflow.backend() {
        Some(backend) => backend,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "SipFlow backend not configured"
                })),
            )
                .into_response();
        }
    };

    // Parse time range
    let now = chrono::Local::now();
    let mut start_time = params.start.and_then(|s| parse_datetime(&s));
    let mut end_time = params.end.and_then(|s| parse_datetime(&s));

    // Try to get time range from call record if not provided
    if (start_time.is_none() || end_time.is_none())
        && let Ok(Some(record)) = CallRecordEntity::find()
            .filter(CallRecordColumn::CallId.eq(&call_id))
            .one(state.db())
            .await
        {
            if start_time.is_none() {
                start_time = Some(
                    record.started_at.with_timezone(&chrono::Local) - chrono::Duration::minutes(10),
                );
            }
            if end_time.is_none() {
                end_time = Some(
                    record
                        .ended_at
                        .unwrap_or(record.started_at)
                        .with_timezone(&chrono::Local)
                        + chrono::Duration::hours(1),
                );
            }
        }

    let start_time = start_time.unwrap_or_else(|| now - chrono::Duration::hours(1));
    let end_time = end_time.unwrap_or(now);

    match backend.query_flow(&call_id, start_time, end_time).await {
        Ok(items) => {
            if items.is_empty() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "Call flow not found"
                    })),
                )
                    .into_response();
            }

            let json_items: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "seq": item.seq,
                        "timestamp": item.timestamp,
                        "msg_type": format!("{:?}", item.msg_type),
                        "src_addr": item.src_addr,
                        "dst_addr": item.dst_addr,
                        "raw_message": String::from_utf8_lossy(&item.payload),
                    })
                })
                .collect();

            Json(serde_json::json!({
                "status": "success",
                "call_id": call_id,
                "start_time": start_time.to_rfc3339(),
                "end_time": end_time.to_rfc3339(),
                "flow": json_items
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to query flow: {}", e)
            })),
        )
            .into_response(),
    }
}

async fn query_sipflow_media(
    State(state): State<AppState>,
    Path(call_id): Path<String>,
    Query(params): Query<SipFlowQueryParams>,
) -> Response {
    use crate::models::call_record::{Column as CallRecordColumn, Entity as CallRecordEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let sip_server = state.sip_server();

    let sipflow = match &sip_server.inner.sip_flow {
        Some(flow) => flow,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "SipFlow not enabled"
                })),
            )
                .into_response();
        }
    };

    let backend = match sipflow.backend() {
        Some(backend) => backend,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "SipFlow backend not configured"
                })),
            )
                .into_response();
        }
    };

    let now = chrono::Local::now();
    let mut start_time = params.start.and_then(|s| parse_datetime(&s));
    let mut end_time = params.end.and_then(|s| parse_datetime(&s));

    // Try to get time range from call record if not provided
    if (start_time.is_none() || end_time.is_none())
        && let Ok(Some(record)) = CallRecordEntity::find()
            .filter(CallRecordColumn::CallId.eq(&call_id))
            .one(state.db())
            .await
        {
            if start_time.is_none() {
                start_time = Some(
                    record.started_at.with_timezone(&chrono::Local) - chrono::Duration::minutes(10),
                );
            }
            if end_time.is_none() {
                end_time = Some(
                    record
                        .ended_at
                        .unwrap_or(record.started_at)
                        .with_timezone(&chrono::Local)
                        + chrono::Duration::hours(1),
                );
            }
        }

    let start_time = start_time.unwrap_or_else(|| now - chrono::Duration::hours(1));
    let end_time = end_time.unwrap_or(now);

    match backend.query_media(&call_id, start_time, end_time).await {
        Ok(data) => {
            if data.is_empty() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "Call media not found"
                    })),
                )
                    .into_response();
            }

            use axum::http::header;

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "audio/wav")
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}.wav\"", call_id),
                )
                .body(axum::body::Body::from(data))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to query media: {}", e)
            })),
        )
            .into_response(),
    }
}

fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::Local>> {
    // Try ISO 8601 format
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Local));
    }

    // Try Unix timestamp
    if let Ok(ts) = s.parse::<i64>()
        && let Some(dt) = chrono::Local.timestamp_opt(ts, 0).single() {
            return Some(dt);
        }

    None
}
