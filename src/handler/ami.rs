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
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::TimeZone;
use futures::stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::{Arc, atomic::Ordering};
use tokio::time::{Duration, sleep};
use tracing::{info, warn};

pub fn ami_router(app_state: AppState) -> Router<AppState> {
    let ami_path = app_state
        .config()
        .proxy
        .ami_path
        .clone()
        .unwrap_or_else(|| "/ami/v1".to_string());
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
        .route("/sipflow/media/{call_id}", get(query_sipflow_media));

    #[cfg(feature = "commerce")]
    let r = r
        .route("/cluster/ping", post(cluster_ping_handler))
        .route("/cluster/reload_config", get(cluster_reload_config_handler))
        .route("/cluster/reload_sync", post(cluster_reload_sync_handler));

    let r = r
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            crate::handler::middleware::ami_auth::ami_auth_middleware,
        ));
    Router::new().nest(&ami_path, r).with_state(app_state)
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

// ── Cluster endpoints ─────────────────────────────────────────────────────────

/// Shared payload type used by both the SSE reload (console) and sync (AMI) endpoints.
#[cfg(feature = "commerce")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct PingReloadPayload {
    #[serde(default)]
    pub trunks: bool,
    #[serde(default)]
    pub routes: bool,
    #[serde(default)]
    pub addons: Vec<String>,
}

#[cfg(feature = "commerce")]
impl<'de> serde::Deserialize<'de> for PingReloadPayload {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};

        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = PingReloadPayload;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a query string with trunks and addons")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut trunks = false;
                let mut routes = false;
                let mut addons: Vec<String> = Vec::new();

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "trunks" => {
                            let v = map.next_value_seed(TrunksSeed)?;
                            trunks = v;
                        }
                        "routes" => {
                            let v = map.next_value_seed(TrunksSeed)?;
                            routes = v;
                        }
                        "addons" => {
                            let v = map.next_value_seed(AddonsSeed)?;
                            addons.extend(v);
                        }
                        _ => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }

                Ok(PingReloadPayload { trunks, routes, addons })
            }
        }

        d.deserialize_map(PayloadVisitor)
    }
}

/// Seed that accepts a boolean or string for the `trunks` field.
#[cfg(feature = "commerce")]
struct TrunksSeed;

#[cfg(feature = "commerce")]
impl<'de> serde::de::DeserializeSeed<'de> for TrunksSeed {
    type Value = bool;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        struct TrunksVisitor;
        impl<'de> serde::de::Visitor<'de> for TrunksVisitor {
            type Value = bool;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a boolean or 'true'/'false' string")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> { Ok(v) }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> { Ok(v == "true" || v == "1") }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> { Ok(v == "true" || v == "1") }
        }
        d.deserialize_any(TrunksVisitor)
    }
}

/// Seed that produces a Vec<String> from either a single string or a sequence.
#[cfg(feature = "commerce")]
struct AddonsSeed;

#[cfg(feature = "commerce")]
impl<'de> serde::de::DeserializeSeed<'de> for AddonsSeed {
    type Value = Vec<String>;

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        struct AddonsVisitor;

        impl<'de> serde::de::Visitor<'de> for AddonsVisitor {
            type Value = Vec<String>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or sequence of strings")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(vec![v.to_string()])
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(vec![v])
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    out.push(s);
                }
                Ok(out)
            }
        }

        d.deserialize_any(AddonsVisitor)
    }
}

/// Ping all configured cluster peers (simple JSON, no SSE needed).
#[cfg(feature = "commerce")]
async fn cluster_ping_handler(State(state): State<AppState>) -> Response {
    use std::time::Instant;

    let peers = state
        .config()
        .cluster
        .as_ref()
        .map(|c| c.peers.clone())
        .unwrap_or_default();

    if peers.is_empty() {
        return Json(serde_json::json!({
            "status": "ok",
            "peers": [],
            "message": "No cluster peers configured",
        }))
        .into_response();
    }

    // Start with current node
    let mut results = vec![serde_json::json!({
        "peer": "current",
        "ami_addr": "local",
        "reachable": true,
        "latency_ms": 0,
        "is_current": true,
    })];

    for peer in &peers {
        let url = format!("http://{}:{}/ami/v1/health", peer.addr, peer.ami_port);
        let start = Instant::now();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        match client.get(&url).send().await {
            Ok(resp) => {
                let latency = start.elapsed().as_millis() as u64;
                results.push(serde_json::json!({
                    "peer": format!("{}:{}", peer.addr, peer.sip_port),
                    "ami_addr": format!("{}:{}", peer.addr, peer.ami_port),
                    "reachable": resp.status().is_success(),
                    "latency_ms": latency,
                    "is_current": false,
                }));
            }
            Err(e) => {
                results.push(serde_json::json!({
                    "peer": format!("{}:{}", peer.addr, peer.sip_port),
                    "ami_addr": format!("{}:{}", peer.addr, peer.ami_port),
                    "reachable": false,
                    "latency_ms": null,
                    "is_current": false,
                    "error": e.to_string(),
                }));
            }
        }
    }

    Json(serde_json::json!({
        "status": "ok",
        "peers": results,
    }))
    .into_response()
}

/// SSE-based reload that processes current node + all peers sequentially.
/// Called with GET and query params (trunks=true&addons=queue&addons=ivr).
/// Uses a channel to stream events from a background task.
#[cfg(feature = "commerce")]
async fn cluster_reload_config_handler(
    State(state): State<AppState>,
    Query(query): Query<PingReloadPayload>,
) -> Response {
    let payload = query;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<SseEvent, Infallible>>();

    tokio::spawn(async move {
        let send_event = |event_type: &'static str, data: serde_json::Value| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Ok(SseEvent::default().event(event_type).data(data.to_string())));
            }
        };

        // Process trunks on current node
        if payload.trunks {
            send_event("progress", serde_json::json!({"type": "addon_start", "node": "current", "addon": "trunks"})).await;
            let result = reload_trunks_on_node(&state, "current").await;
            send_event("progress", serde_json::json!({"type": "addon_complete", "node": "current", "addon": "trunks", "result": result})).await;
        }

        // Process routes on current node
        if payload.routes {
            send_event("progress", serde_json::json!({"type": "addon_start", "node": "current", "addon": "routes"})).await;
            let result = reload_routes_on_node(&state, "current").await;
            send_event("progress", serde_json::json!({"type": "addon_complete", "node": "current", "addon": "routes", "result": result})).await;
        }

        // Process addon-based handlers on current node
        for addon_id in &payload.addons {
            send_event("progress", serde_json::json!({"type": "addon_start", "node": "current", "addon": addon_id})).await;
            let results = state.addon_registry.export_reload
                .invoke_selected(&[addon_id.clone()], &state)
                .await;
            let json_result = match results.into_iter().next() {
                Some((_, Ok(v))) => serde_json::json!({ "status": "ok", "details": v }),
                Some((_, Err(e))) => serde_json::json!({ "status": "error", "message": e }),
                None => serde_json::json!({ "status": "error", "message": "Handler not found" }),
            };
            send_event("progress", serde_json::json!({"type": "addon_complete", "node": "current", "addon": addon_id, "result": json_result})).await;
        }

        // Process peer nodes
        let peers = state.config().cluster.as_ref()
            .map(|c| c.peers.clone())
            .unwrap_or_default();

        for peer in &peers {
            let peer_label = format!("{}:{}", peer.addr, peer.ami_port);
            send_event("progress", serde_json::json!({"type": "node_start", "node": &peer_label})).await;

            let url = format!("http://{}:{}/ami/v1/cluster/reload_sync", peer.addr, peer.ami_port);
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default();

            match client.post(&url).json(&payload).send().await {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(peer_results) => {
                        send_event("progress", serde_json::json!({"type": "node_complete", "node": &peer_label, "result": peer_results})).await;
                    }
                    Err(e) => {
                        send_event("progress", serde_json::json!({"type": "node_error", "node": &peer_label, "error": format!("Invalid response: {}", e)})).await;
                    }
                },
                Err(e) => {
                    send_event("progress", serde_json::json!({"type": "node_error", "node": &peer_label, "error": format!("Connection failed: {}", e)})).await;
                }
            }
        }

        send_event("complete", serde_json::json!({"type": "complete"})).await;
    });

    let stream = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive"))
        .into_response()
}

/// Synchronous JSON reload endpoint for peer-to-peer communication.
/// Processes only the current node and returns JSON results.

/// Synchronous JSON reload endpoint for peer-to-peer communication.
/// Processes only the current node and returns JSON results.
#[cfg(feature = "commerce")]
async fn cluster_reload_sync_handler(
    State(state): State<AppState>,
    Json(payload): Json<PingReloadPayload>,
) -> Response {
    let mut results: Vec<serde_json::Value> = Vec::new();

    if payload.trunks {
        let r = reload_trunks_on_node(&state, "current").await;
        results.push(r);
    }

    if payload.routes {
        let r = reload_routes_on_node(&state, "current").await;
        results.push(r);
    }

    for addon_id in &payload.addons {
        let result = state.addon_registry.export_reload
            .invoke_selected(&[addon_id.clone()], &state)
            .await;
        let json_result = match result.into_iter().next() {
            Some((_, Ok(v))) => serde_json::json!({ "addon": addon_id, "status": "ok", "details": v }),
            Some((_, Err(e))) => serde_json::json!({ "addon": addon_id, "status": "error", "message": e }),
            None => serde_json::json!({ "addon": addon_id, "status": "error", "message": "Handler not found" }),
        };
        results.push(json_result);
    }

    Json(serde_json::json!({ "status": "completed", "results": results })).into_response()
}

// ── Shared helpers ────────────────────────────────────────────────────────────

#[cfg(feature = "commerce")]
async fn reload_trunks_on_node(state: &AppState, _node: &str) -> serde_json::Value {
    let config_override = match load_proxy_config_override(state) {
        Ok(cfg) => cfg,
        Err(_) => {
            return serde_json::json!({ "addon": "trunks", "status": "error", "message": "Failed to load config" });
        }
    };
    match state
        .sip_server()
        .inner
        .data_context
        .reload_trunks(true, config_override)
        .await
    {
        Ok(metrics) => {
            if let Some(ref console) = state.console {
                console.clear_pending_reload();
            }
            serde_json::json!({ "addon": "trunks", "status": "ok", "reloaded": metrics.total })
        }
        Err(e) => serde_json::json!({ "addon": "trunks", "status": "error", "message": e.to_string() }),
    }
}

#[cfg(feature = "commerce")]
async fn reload_routes_on_node(state: &AppState, _node: &str) -> serde_json::Value {
    let config_override = match load_proxy_config_override(state) {
        Ok(cfg) => cfg,
        Err(_) => {
            return serde_json::json!({ "addon": "routes", "status": "error", "message": "Failed to load config" });
        }
    };
    match state
        .sip_server()
        .inner
        .data_context
        .reload_routes(true, config_override)
        .await
    {
        Ok(metrics) => {
            if let Some(ref console) = state.console {
                console.clear_pending_reload();
            }
            serde_json::json!({ "addon": "routes", "status": "ok", "reloaded": metrics.total })
        }
        Err(e) => serde_json::json!({ "addon": "routes", "status": "error", "message": e.to_string() }),
    }
}

#[cfg(feature = "commerce")]
fn make_sse_event(event_type: &'static str, data: serde_json::Value) -> Result<SseEvent, Infallible> {
    Ok(SseEvent::default().event(event_type).data(data.to_string()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(feature = "commerce")]
mod cluster_tests {
    use super::*;

    #[test]
    fn test_deserialize_ping_payload_single_addon() {
        let qs = "trunks=true&addons=ivr";
        let payload: PingReloadPayload = serde_urlencoded::from_str(qs).unwrap();
        assert!(payload.trunks);
        assert_eq!(payload.addons, vec!["ivr"]);
    }

    #[test]
    fn test_deserialize_ping_payload_multi_addon() {
        let qs = "trunks=false&addons=queue&addons=ivr";
        let payload: PingReloadPayload = serde_urlencoded::from_str(qs).unwrap();
        assert!(!payload.trunks);
        assert_eq!(payload.addons, vec!["queue", "ivr"]);
    }

    #[test]
    fn test_deserialize_ping_payload_no_addons() {
        let qs = "trunks=true";
        let payload: PingReloadPayload = serde_urlencoded::from_str(qs).unwrap();
        assert!(payload.trunks);
        assert!(payload.addons.is_empty());
    }

    #[test]
    fn test_deserialize_ping_payload_empty() {
        let qs = "";
        let payload: PingReloadPayload = serde_urlencoded::from_str(qs).unwrap();
        assert!(!payload.trunks);
        assert!(payload.addons.is_empty());
    }

    #[test]
    fn test_json_roundtrip() {
        // Use JSON for roundtrip instead of serde_urlencoded which has
        // known limitations with Vec serialization.
        let payload = PingReloadPayload { trunks: true, routes: false, addons: vec!["queue".into(), "ivr".into()] };
        let json_str = serde_json::to_string(&payload).unwrap();
        let decoded: PingReloadPayload = serde_json::from_str(&json_str).unwrap();
        assert_eq!(decoded.trunks, true);
        assert_eq!(decoded.addons, vec!["queue", "ivr"]);
    }
}
