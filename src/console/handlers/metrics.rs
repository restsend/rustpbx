use crate::console::ConsoleState;
use crate::console::middleware::AuthRequired;
use axum::{Json, extract::State, http::HeaderMap, response::Response};
use serde_json::json;
use std::sync::Arc;

/// Render the metrics dashboard page.
pub async fn metrics_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let runtime_metrics = collect_runtime_metrics(&state).await;

    // Get Prometheus metrics endpoint configuration
    let prometheus_config = get_prometheus_config(&state);
    let current_user = state.build_current_user_ctx(&user).await;

    state.render_with_headers(
        "console/metrics.html",
        json!({
            "nav_active": "metrics",
            "metrics": runtime_metrics,
            "prometheus": prometheus_config,
            "current_user": current_user,
        }),
        &headers,
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
    if let Some(app_state) = state.app_state() {
        if let Some(info) = app_state
            .addon_registry
            .metrics_endpoint_info(&app_state.config_path)
        {
            return PrometheusConfig {
                enabled: info.enabled,
                path: info.path,
                auth_required: info.auth_required,
            };
        }
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
    Json(collect_runtime_metrics(&state).await)
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
    /// Transaction pressure metrics
    pub transaction: TransactionMetrics,
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
    /// Live media bridges (conference / recording sessions)
    pub active_bridges: usize,
    /// RTP packets received (process lifetime)
    pub rx_packets_total: u64,
    /// RTP packets lost in receive direction
    pub rx_packets_lost: u64,
    /// RTP packets sent (process lifetime)
    pub tx_packets_total: u64,
    /// RTP packets lost in send direction (from RTCP feedback)
    pub tx_packets_lost: u64,
}

#[derive(Clone, serde::Serialize, Default)]
pub struct VoicemailMetrics {
    /// Total messages today
    pub messages_today: u64,
    /// Active mailboxes
    pub active_mailboxes: u32,
}

#[derive(Clone, serde::Serialize, Default)]
pub struct TransactionMetrics {
    /// Currently running transactions (user-space counter)
    pub running: u32,
    /// Max concurrency limit (0 = unlimited)
    pub max_concurrency: u32,
    /// RSIP endpoint internal running transactions
    pub endpoint_running: usize,
    /// RSIP endpoint cumulative finished transactions
    pub endpoint_finished: usize,
    /// RSIP endpoint transactions waiting for ACK
    pub endpoint_waiting_ack: usize,
}

async fn collect_runtime_metrics(state: &ConsoleState) -> RuntimeMetrics {
    let system = collect_system_metrics(state);
    let sip = collect_sip_metrics(state);
    let calls = collect_call_metrics(state);
    let transaction = collect_transaction_metrics(state);
    let media = collect_media_metrics(state).await;
    let voicemail = collect_voicemail_metrics(state).await;

    RuntimeMetrics {
        system,
        sip,
        calls,
        transaction,
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
        metrics.capacity = server.proxy_config.load().max_concurrency.unwrap_or(0) as u32;
    }

    metrics.utilization = if metrics.capacity > 0 {
        ((metrics.active as f64 / metrics.capacity as f64) * 100.0).round() as u32
    } else {
        0
    };

    metrics
}

fn collect_transaction_metrics(state: &ConsoleState) -> TransactionMetrics {
    let mut metrics = TransactionMetrics::default();

    if let Some(server) = state.sip_server() {
        metrics.running = server
            .runnings_tx
            .load(std::sync::atomic::Ordering::Relaxed) as u32;
        metrics.max_concurrency = server.proxy_config.load().max_concurrency.unwrap_or(0) as u32;
        let stats = server.endpoint.inner.get_stats();
        metrics.endpoint_running = stats.running_transactions;
        metrics.endpoint_finished = stats.finished_transactions;
        metrics.endpoint_waiting_ack = stats.waiting_ack;
    }

    metrics
}

/// Media metrics from the process-wide telemetry aggregate and the
/// registration locator (previously hardcoded to an empty struct, which left
/// the metrics dashboard blind to RTP health).
async fn collect_media_metrics(state: &ConsoleState) -> MediaMetrics {
    let snapshot = rustpbx_media::telemetry::MediaTelemetry::snapshot();
    let mut metrics = MediaMetrics {
        active_bridges: snapshot.active_bridges,
        rx_packets_total: snapshot.rx.packets_total,
        rx_packets_lost: snapshot.rx.lost_total,
        tx_packets_total: snapshot.tx.packets_total,
        tx_packets_lost: snapshot.tx.lost_total,
        ..Default::default()
    };
    if let Some(server) = state.sip_server()
        && let Ok(stats) = server.locator.online_stats().await
    {
        metrics.webrtc_connections = stats.webrtc_locations as u64;
    }
    metrics
}

/// Voicemail counters read from the voicemail addon tables. Zero unless the
/// voicemail addon is enabled: its tables only exist once it has run, and
/// querying them otherwise fails (Postgres logs every such attempt).
async fn collect_voicemail_metrics(state: &ConsoleState) -> VoicemailMetrics {
    let Some(app) = state.app_state() else {
        return VoicemailMetrics::default();
    };
    if !app.addon_registry.is_enabled("voicemail", app.config()) {
        return VoicemailMetrics::default();
    }
    let today_start = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    query_voicemail_metrics(app.db(), today_start).await
}

async fn query_voicemail_metrics<C: sea_orm::ConnectionTrait>(
    db: &C,
    today_start: chrono::DateTime<chrono::Utc>,
) -> VoicemailMetrics {
    use sea_orm::ExprTrait;
    use sea_orm::sea_query::{Alias, Asterisk, Expr, Query};

    let mut metrics = VoicemailMetrics::default();
    let backend = db.get_database_backend();
    let count = |table: &str| {
        Query::select()
            .expr_as(Expr::col(Asterisk).count(), Alias::new("c"))
            .from(Alias::new(table))
            .to_owned()
    };

    // COUNT queries built per backend (placeholders differ); a missing table
    // (addon disabled) errors and the metric stays at its default of 0.
    if let Some(row) = db
        .query_one_raw(backend.build(&count("rustpbx_voicemail_box")))
        .await
        .ok()
        .flatten()
    {
        metrics.active_mailboxes =
            std::cmp::Ord::max(row.try_get::<i64>("", "c").unwrap_or(0), 0) as u32;
    }
    let today = count("rustpbx_voicemail_message")
        .and_where(Expr::col(Alias::new("created_at")).gte(today_start))
        .to_owned();
    if let Some(row) = db.query_one_raw(backend.build(&today)).await.ok().flatten() {
        metrics.messages_today =
            std::cmp::Ord::max(row.try_get::<i64>("", "c").unwrap_or(0), 0) as u64;
    }
    metrics
}

/// Page routes (nested under base_path)
pub fn urls() -> axum::Router<Arc<ConsoleState>> {
    use axum::routing::get;
    axum::Router::new().route("/metrics/runtime", get(metrics_page))
}

/// API routes (nested under api_prefix)
pub fn api_urls() -> axum::Router<Arc<ConsoleState>> {
    use axum::routing::get;
    axum::Router::new().route("/metrics/runtime/data", get(metrics_data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::domain_report::tests::{
        fixture_day, mysql_db, postgres_db, seed_voicemail, sqlite_db,
    };
    use sea_orm::{ConnectionTrait, DatabaseConnection};

    async fn assert_voicemail_metrics(db: &DatabaseConnection) {
        seed_voicemail(db).await;
        let metrics = query_voicemail_metrics(db, fixture_day()).await;
        assert_eq!(metrics.active_mailboxes, 2);
        assert_eq!(metrics.messages_today, 2);
    }

    #[tokio::test]
    async fn test_voicemail_metrics_sqlite() {
        assert_voicemail_metrics(&sqlite_db().await).await;
    }

    #[tokio::test]
    async fn test_voicemail_metrics_without_addon_tables() {
        let metrics = query_voicemail_metrics(&sqlite_db().await, fixture_day()).await;
        assert_eq!(metrics.active_mailboxes, 0);
        assert_eq!(metrics.messages_today, 0);
    }

    #[tokio::test]
    #[ignore = "requires RUSTPBX_POSTGRES_TEST_URL pointing to a PostgreSQL test database"]
    async fn test_voicemail_metrics_postgres() {
        let db = postgres_db("voicemail_metrics_test").await;
        assert_voicemail_metrics(&db).await;
        db.execute_unprepared("DROP SCHEMA voicemail_metrics_test CASCADE")
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires RUSTPBX_MYSQL_TEST_URL pointing to a MySQL test server"]
    async fn test_voicemail_metrics_mysql() {
        let db = mysql_db("rustpbx_voicemail_metrics_test").await;
        assert_voicemail_metrics(&db).await;
        db.execute_unprepared("DROP DATABASE rustpbx_voicemail_metrics_test")
            .await
            .unwrap();
    }
}
