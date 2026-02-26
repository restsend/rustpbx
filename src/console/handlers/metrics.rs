use crate::console::ConsoleState;
use crate::console::middleware::AuthRequired;
use axum::{Json, extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

/// Render the metrics dashboard page.
pub async fn metrics_page(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let runtime_metrics = collect_runtime_metrics(&state);

    // Get Prometheus metrics endpoint configuration
    let prometheus_config = get_prometheus_config(&state);

    state.render(
        "console/metrics.html",
        json!({
            "nav_active": "metrics",
            "metrics": runtime_metrics,
            "prometheus": prometheus_config,
        }),
    )
}

/// Prometheus endpoint configuration for UI
#[derive(Clone, serde::Serialize)]
struct PrometheusConfig {
    /// Whether the observability addon is enabled
    pub enabled: bool,
    /// The metrics endpoint path (e.g., "/metrics")
    pub path: String,
    /// Whether authentication is required
    pub auth_required: bool,
}

fn get_prometheus_config(state: &ConsoleState) -> PrometheusConfig {
    // Check if metrics are configured
    if let Some(app_state) = state.app_state() {
        let cfg = app_state.config().metrics.clone().unwrap_or_default();
        return PrometheusConfig {
            enabled: cfg.enabled,
            path: cfg.path,
            auth_required: cfg.token.is_some(),
        };
    }

    PrometheusConfig {
        enabled: false,
        path: "/metrics".to_string(),
        auth_required: false,
    }
}

/// API endpoint to get runtime metrics as JSON.
pub async fn metrics_data(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Json<RuntimeMetrics> {
    Json(collect_runtime_metrics(&state))
}

/// Runtime metrics collected from various sources.
#[derive(Clone, serde::Serialize)]
pub struct RuntimeMetrics {
    /// System metrics (uptime, memory, etc.)
    pub system: SystemMetrics,
    /// SIP layer metrics
    pub sip: SipMetrics,
    /// Call metrics
    pub calls: CallMetrics,
    /// Media metrics
    pub media: MediaMetrics,
    /// Voicemail metrics
    pub voicemail: VoicemailMetrics,
    /// Timestamp when metrics were collected
    pub collected_at: String,
}

#[derive(Clone, serde::Serialize)]
pub struct SystemMetrics {
    pub uptime_seconds: i64,
    pub version: String,
    pub edition: String,
}

#[derive(Clone, serde::Serialize, Default)]
pub struct SipMetrics {
    /// Current registered endpoints count
    pub registrations_active: u32,
    /// Total registration attempts
    pub registrations_total: u64,
    /// Successful registrations
    pub registrations_succeeded: u64,
    /// Failed registrations
    pub registrations_failed: u64,
    /// Active SIP dialogs
    pub dialogs_active: u32,
}

#[derive(Clone, serde::Serialize, Default)]
pub struct CallMetrics {
    /// Active calls right now
    pub active: u32,
    /// Call capacity
    pub capacity: u32,
    /// Utilization percentage
    pub utilization: u32,
}

#[derive(Clone, serde::Serialize, Default)]
pub struct MediaMetrics {
    /// WebRTC connections
    pub webrtc_connections: u64,
}

#[derive(Clone, serde::Serialize, Default)]
pub struct VoicemailMetrics {
    /// Total messages today
    pub messages_today: u64,
    /// Active mailboxes
    pub active_mailboxes: u32,
}

fn collect_runtime_metrics(state: &ConsoleState) -> RuntimeMetrics {
    let system = collect_system_metrics(state);
    let sip = collect_sip_metrics(state);
    let calls = collect_call_metrics(state);
    let media = MediaMetrics::default();
    let voicemail = VoicemailMetrics::default();

    RuntimeMetrics {
        system,
        sip,
        calls,
        media,
        voicemail,
        collected_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn collect_system_metrics(state: &ConsoleState) -> SystemMetrics {
    let uptime_seconds = state
        .app_state()
        .map(|s| (chrono::Utc::now() - s.uptime).num_seconds())
        .unwrap_or(0);

    let version = crate::version::get_short_version().to_string();
    let edition = if cfg!(feature = "commerce") {
        "commerce".to_string()
    } else {
        "community".to_string()
    };

    SystemMetrics {
        uptime_seconds,
        version,
        edition,
    }
}

fn collect_sip_metrics(state: &ConsoleState) -> SipMetrics {
    let mut metrics = SipMetrics::default();

    if let Some(server) = state.sip_server() {
        // Count active dialogs
        metrics.dialogs_active = server.active_call_registry.count() as u32;
    }

    metrics
}

fn collect_call_metrics(state: &ConsoleState) -> CallMetrics {
    let mut metrics = CallMetrics::default();

    if let Some(server) = state.sip_server() {
        metrics.active = server.active_call_registry.count() as u32;
        metrics.capacity = server.proxy_config.max_concurrency.unwrap_or(0) as u32;
    }

    metrics.utilization = if metrics.capacity > 0 {
        ((metrics.active as f64 / metrics.capacity as f64) * 100.0).round() as u32
    } else {
        0
    };

    metrics
}

/// URL routes for metrics module.
pub fn urls() -> axum::Router<Arc<ConsoleState>> {
    use axum::routing::get;

    axum::Router::new()
        .route("/metrics/runtime", get(metrics_page))
        .route("/metrics/runtime/data", get(metrics_data))
}
