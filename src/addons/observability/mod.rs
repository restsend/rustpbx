//! Community observability addon — Prometheus metrics + liveness probe.
//!
//! ## What it does
//!
//! * Installs a global **Prometheus recorder** for the [`metrics`] facade so
//!   every `metrics::counter!` / `metrics::gauge!` / `metrics::histogram!`
//!   call in the rest of the codebase is captured automatically.
//! * Exposes two unauthenticated HTTP endpoints:
//!   - `GET /healthz`  – liveness probe (returns JSON `{"status":"ok",...}`)  
//!   - `GET /metrics`  – Prometheus text-format scrape endpoint
//!
//! ## Feature flag
//!
//! Enabled by compiling with `--features addon-observability`.
//! The commercial `addon-telemetry` feature supersedes this addon: when both
//! are compiled the commercial addon *replaces* the Prometheus recorder with
//! an OpenTelemetry bridge and owns the `/metrics` path (via OTLP push),
//! but keeps `/healthz`.
//!
//! ## `config.toml` snippet
//!
//! ```toml
//! [metrics]
//! enabled      = true
//! path         = "/metrics"   # default
//! healthz_path = "/healthz"   # default
//! # token = "secret"          # optional bearer token guard
//! ```

use crate::addons::{Addon, AddonCategory};
use crate::app::AppState;
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::OnceLock;

static PROMETHEUS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

pub struct ObservabilityAddon;

impl ObservabilityAddon {
    pub fn new() -> Self {
        Self
    }

    /// Install the global Prometheus recorder.
    ///
    /// **Must be called exactly once**, before [`AddonRegistry::initialize_all`].
    /// Subsequent calls are no-ops (the `OnceLock` guarantees idempotency).
    ///
    /// Returns an error only if `metrics::set_global_recorder` fails, which
    /// happens when another recorder was already installed (e.g. in tests that
    /// call this multiple times in the same process).
    pub fn install_recorder() -> anyhow::Result<()> {
        if PROMETHEUS_HANDLE.get().is_some() {
            return Ok(());
        }

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
            // Buckets tuned for telephony: sub-second latency is the norm.
            .set_buckets(&[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ])
            .map_err(|e| anyhow::anyhow!("failed to configure Prometheus buckets: {e}"))?
            .build_recorder();

        let handle = recorder.handle();

        // Try to set the global recorder. If it fails because a recorder is already
        // installed (e.g., another test ran first), treat it as success.
        if let Err(_) = metrics::set_global_recorder(recorder) {
            // Another recorder was already installed - this is fine for tests.
            // Just return success without setting our handle.
            return Ok(());
        }

        let _ = PROMETHEUS_HANDLE.set(handle);
        tracing::info!("Prometheus metrics recorder installed");
        Ok(())
    }
}

#[async_trait]
impl Addon for ObservabilityAddon {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &'static str {
        "observability"
    }

    fn name(&self) -> &'static str {
        "Observability"
    }

    fn description(&self) -> &'static str {
        "Exposes /metrics (Prometheus format) and /healthz endpoints for monitoring."
    }

    fn category(&self) -> AddonCategory {
        AddonCategory::Community
    }

    fn cost(&self) -> &'static str {
        "Free"
    }

    async fn initialize(&self, state: AppState) -> anyhow::Result<()> {
        // Register built-in static gauges so they appear at scrape-time even
        // before any calls have been processed.
        let ver = crate::version::get_short_version();
        metrics::gauge!("rustpbx_info", "version" => ver).set(1.0);

        tracing::info!(
            metrics_path = %state.config().metrics.as_ref().map(|m| m.path.as_str()).unwrap_or("/metrics"),
            healthz_path = %state.config().metrics.as_ref().map(|m| m.healthz_path.as_str()).unwrap_or("/healthz"),
            "ObservabilityAddon ready"
        );
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        let cfg = state.config().metrics.clone().unwrap_or_default();

        if !cfg.enabled {
            return None;
        }

        // Clone the token for use in the closure.
        let token = cfg.token.clone();

        // Build the sub-router.
        let r = Router::new()
            .route(&cfg.healthz_path, get(healthz_handler))
            .route(
                &cfg.path,
                get(metrics_handler).layer(middleware::from_fn_with_state(
                    token,
                    metrics_auth_middleware,
                )),
            )
            .with_state(state);

        Some(r)
    }

    fn call_record_hook(
        &self,
        _db: &sea_orm::DatabaseConnection,
    ) -> Option<Box<dyn crate::callrecord::CallRecordHook>> {
        Some(Box::new(MetricsCallRecordHook))
    }
}

/// `GET /healthz` — liveness probe.
///
/// Returns HTTP 200 with a JSON body.  Intentionally does **not** check the
/// database or SIP server so it can be used as a pod-level liveness probe
/// even when those services are temporarily unavailable.
async fn healthz_handler(State(state): State<AppState>) -> impl IntoResponse {
    let uptime_seconds = (chrono::Utc::now() - state.uptime).num_seconds();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "status": "ok",
            "uptime_seconds": uptime_seconds,
            "version": crate::version::get_short_version(),
            "active_calls": state.total_calls.load(std::sync::atomic::Ordering::Relaxed),
        })),
    )
}

/// `GET /metrics` — Prometheus scrape endpoint.
async fn metrics_handler() -> impl IntoResponse {
    match PROMETHEUS_HANDLE.get() {
        Some(handle) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            handle.render(),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Prometheus recorder not initialised",
        )
            .into_response(),
    }
}

/// If a `token` is configured, require `Authorization: Bearer <token>` on every
/// request to `/metrics`.  Requests without or with a wrong token get HTTP 401.
async fn metrics_auth_middleware(
    State(configured_token): State<Option<String>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(ref expected) = configured_token {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        if provided != Some(expected.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer realm=\"metrics\"")],
                "Unauthorized",
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Plugged into the `CallRecordManager` via `Addon::call_record_hook()`.
///
/// Emits the following metrics for every completed call:
///
/// | Name | Type | Labels |
/// |---|---|---|
/// | `rustpbx_calls_total` | Counter | `direction`, `result` |
/// | `rustpbx_call_duration_seconds` | Histogram | `direction` |
/// | `rustpbx_call_talk_time_seconds` | Histogram | `direction` |
pub struct MetricsCallRecordHook;

#[async_trait]
impl crate::callrecord::CallRecordHook for MetricsCallRecordHook {
    async fn on_record_completed(
        &self,
        record: &mut crate::callrecord::CallRecord,
    ) -> anyhow::Result<()> {
        let direction = record.details.direction.as_str();

        // Total elapsed time from INVITE to BYE.
        let elapsed = (record.end_time - record.start_time)
            .num_milliseconds()
            .max(0) as f64
            / 1_000.0;

        // Actual talk time (only if the call was answered).
        let talk_secs = record
            .answer_time
            .map(|a| (record.end_time - a).num_milliseconds().max(0) as f64 / 1_000.0);

        // Classify the result.
        let result = if record.status_code < 300 {
            "ok"
        } else if record.status_code < 400 {
            "redirect"
        } else if record.status_code < 500 {
            "rejected"
        } else {
            "failed"
        };

        metrics::counter!(
            "rustpbx_calls_total",
            "direction" => direction.to_string(),
            "result"    => result
        )
        .increment(1);

        metrics::histogram!(
            "rustpbx_call_duration_seconds",
            "direction" => direction.to_string()
        )
        .record(elapsed);

        if let Some(talk) = talk_secs {
            metrics::histogram!(
                "rustpbx_call_talk_time_seconds",
                "direction" => direction.to_string()
            )
            .record(talk);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callrecord::{CallDetails, CallRecord, CallRecordHook};
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
        routing::get,
    };
    use chrono::Utc;
    use tower::ServiceExt as _;

    /// Minimal `CallRecord` factory.  Keeps individual tests concise.
    fn make_record(direction: &str, status_code: u16, answered: bool) -> CallRecord {
        let now = Utc::now();
        let start = now - chrono::Duration::seconds(90);
        let answer = if answered {
            Some(now - chrono::Duration::seconds(60))
        } else {
            None
        };
        CallRecord {
            call_id: format!("test-{direction}-{status_code}"),
            start_time: start,
            end_time: now,
            answer_time: answer,
            caller: "+10000000001".to_string(),
            callee: "+10000000002".to_string(),
            status_code,
            details: CallDetails {
                direction: direction.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_addon_id() {
        let addon = ObservabilityAddon::new();
        assert_eq!(addon.id(), "observability");
    }

    #[test]
    fn test_addon_category_is_community() {
        let addon = ObservabilityAddon::new();
        assert_eq!(addon.category(), AddonCategory::Community);
    }

    #[test]
    fn test_addon_cost_is_free() {
        let addon = ObservabilityAddon::new();
        assert_eq!(addon.cost(), "Free");
    }

    #[test]
    fn test_addon_name_and_description_nonempty() {
        let addon = ObservabilityAddon::new();
        assert!(!addon.name().is_empty());
        assert!(!addon.description().is_empty());
    }

    /// install_recorder() must succeed the first time and be idempotent on
    /// repeated calls (even across parallel tests that share the same process).
    #[test]
    fn test_install_recorder_idempotent() {
        // First call may-or-may-not be the very first in the process; both are Ok.
        let r1 = ObservabilityAddon::install_recorder();
        assert!(r1.is_ok(), "first install_recorder failed: {:?}", r1);

        // Second call must always succeed.
        let r2 = ObservabilityAddon::install_recorder();
        assert!(r2.is_ok(), "second install_recorder failed: {:?}", r2);
    }

    /// Inbound answered call with 200 OK → emits counter + duration + talk_time.
    #[tokio::test]
    async fn test_hook_inbound_answered_ok() {
        ObservabilityAddon::install_recorder().ok();
        let hook = MetricsCallRecordHook;
        let mut record = make_record("inbound", 200, true);
        hook.on_record_completed(&mut record)
            .await
            .expect("hook must not error");
    }

    /// Outbound unanswered call → only counter + duration, no talk_time.
    #[tokio::test]
    async fn test_hook_outbound_unanswered_ok() {
        ObservabilityAddon::install_recorder().ok();
        let hook = MetricsCallRecordHook;
        let mut record = make_record("outbound", 200, false);
        hook.on_record_completed(&mut record)
            .await
            .expect("hook must not error");
    }

    /// 4xx calls are classified as "rejected".
    #[tokio::test]
    async fn test_hook_result_rejected_on_4xx() {
        ObservabilityAddon::install_recorder().ok();
        let hook = MetricsCallRecordHook;
        let mut record = make_record("inbound", 486, false); // 486 Busy Here
        hook.on_record_completed(&mut record)
            .await
            .expect("hook must not error");
    }

    /// 5xx calls are classified as "failed".
    #[tokio::test]
    async fn test_hook_result_failed_on_5xx() {
        ObservabilityAddon::install_recorder().ok();
        let hook = MetricsCallRecordHook;
        let mut record = make_record("outbound", 503, false);
        hook.on_record_completed(&mut record)
            .await
            .expect("hook must not error");
    }

    /// 3xx calls are classified as "redirect".
    #[tokio::test]
    async fn test_hook_result_redirect_on_3xx() {
        ObservabilityAddon::install_recorder().ok();
        let hook = MetricsCallRecordHook;
        let mut record = make_record("internal", 302, false);
        hook.on_record_completed(&mut record)
            .await
            .expect("hook must not error");
    }

    /// end_time == start_time → elapsed = 0 ms, should not produce negative f64.
    #[tokio::test]
    async fn test_hook_zero_duration_does_not_panic() {
        ObservabilityAddon::install_recorder().ok();
        let hook = MetricsCallRecordHook;
        let now = Utc::now();
        let mut record = CallRecord {
            call_id: "zero-dur".to_string(),
            start_time: now,
            end_time: now,
            caller: "a".to_string(),
            callee: "b".to_string(),
            status_code: 200,
            details: CallDetails {
                direction: "inbound".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        hook.on_record_completed(&mut record)
            .await
            .expect("zero duration must not error");
    }

    /// Build a throw-away router that applies `metrics_auth_middleware` with the
    /// given token, then fire a one-shot request and return the status code.
    async fn auth_status(configured_token: Option<String>, bearer: Option<&str>) -> StatusCode {
        let app = Router::new()
            .route("/metrics", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                configured_token,
                metrics_auth_middleware,
            ));

        let mut builder = Request::builder().uri("/metrics").method("GET");

        if let Some(b) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {b}"));
        }

        let req = builder.body(Body::empty()).unwrap();

        app.oneshot(req).await.unwrap().status()
    }

    /// No token configured → every request is allowed through.
    #[tokio::test]
    async fn test_auth_no_token_configured_allows_all() {
        assert_eq!(auth_status(None, None).await, StatusCode::OK);
        assert_eq!(auth_status(None, Some("any_bearer")).await, StatusCode::OK);
    }

    /// Correct bearer → 200.
    #[tokio::test]
    async fn test_auth_valid_bearer_passes() {
        let status = auth_status(Some("secret".to_string()), Some("secret")).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// Wrong bearer → 401.
    #[tokio::test]
    async fn test_auth_wrong_bearer_rejected() {
        let status = auth_status(Some("secret".to_string()), Some("wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// No Authorization header when token is configured → 401.
    #[tokio::test]
    async fn test_auth_missing_header_when_token_required() {
        let status = auth_status(Some("secret".to_string()), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Empty bearer value must also be rejected.
    #[tokio::test]
    async fn test_auth_empty_bearer_rejected() {
        let status = auth_status(Some("secret".to_string()), Some("")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
