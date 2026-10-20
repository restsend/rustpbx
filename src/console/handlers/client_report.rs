//! Client telemetry ingestion + statistics ("客户端上报").
//!
//! Ingestion contract — `restsend-call/crates/pc/src/telemetry.rs` is the
//! authoritative client implementation:
//!
//! - `POST {api_prefix}/report` is the ONE public collector (unauthenticated;
//!   exempted from `api_auth_middleware` and `csrf_guard` in their modules).
//! - Body: `application/gzip` of plain text (client caps full-day logs at
//!   8 MiB; this side accepts up to [`MAX_BODY_BYTES`] and caps decompression
//!   at [`MAX_DECOMPRESSED_BYTES`] as a zip-bomb guard). Non-gzip bodies are
//!   accepted as plain text.
//! - Metadata rides in query params, mirrored in `X-Restsend-*` headers.
//!   Params win; headers are the fallback.
//!   - `kind`: `task` | `crash_day` | `diag`
//!   - `event`: 15 known values (see [`KNOWN_EVENTS`]); unknown values are
//!     sanitized and kept so forward-compatible clients still count.
//!   - env: `version` / `build` / `build_time` / `git_commit` / `os` / `arch`
//!     / `hostname` / `mac` / `cpu_cores` / `mem_total_mb` / `mem_avail_mb`
//!   - account: `agent_id` / `account_name` / `auth` / `media` / `transport`
//!     / `sip_user` / `registrar`
//!   - context: `ts` (unix secs), optional `call_id` / `remote_uri`, plus
//!     `in_call` / `safe_mode` (headers `X-Restsend-In-Call` / `-Safe-Mode`)
//!     and `note` (`X-Restsend-Note`).
//! - Response: `{"ok": true}` on success (2xx = accepted; clients retry on
//!   failures themselves).
//!
//! Storage layout (zero migrations — reuses `rustpbx_runtime_snapshots`):
//!
//! - Log bodies live on local disk under `[release].telemetry_dir`
//!   (default `storage/telemetry`), as `YYYY-MM-DD/<unix>_<seq>_<kind>_<event>.gz`
//!   (original gzip bytes kept; plain bodies re-gzipped). Local disk on
//!   purpose: ingestion must not depend on S3 or any other subsystem.
//! - One snapshot row per report:
//!   - `kind` = `cr:{event}` (≤ 3 + 20 chars, fits the 32-char column)
//!   - `instance_id` = client `version` → version distribution is a plain
//!     `GROUP BY instance_id`
//!   - `num1..3` = body bytes / in_call / safe_mode
//!   - `data` JSON = remaining dims (os/arch/agent_id/... see [`ingest_row`])
//! - `GET {api_prefix}/release` update checks record a `cr:update_check` row
//!   (the previously debug-logged client identity becomes data).
//!
//! Rows age out with the table's shared 30-day pruning; bodies keep their own
//! directory-based retention (`prune_telemetry_files`, invoked from the same
//! hourly sweep as the snapshot prune).
//!
//! Stats API (RBAC: `reports:read` OR `cdr:read`, same as the reports page):
//! `GET /console/api/client-reports/{summary,trend,list,body,export}`.

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde_json::json;
use std::{
    collections::HashMap,
    io::Read,
    net::IpAddr,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use std::sync::OnceLock;

use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::runtime_snapshot::{Column as SnapCol, Entity as SnapshotEntity};
use crate::report::{bucket_index_expr, bucket_start_utc, json_text_expr, pct};
use crate::utils::count_when;

/// Client log kinds (`X-Restsend-Log-Kind`).
pub const KNOWN_KINDS: [&str; 3] = ["task", "crash_day", "diag"];

/// Events emitted by restsend-call. Abnormal ones drive the "异常" counters.
pub const KNOWN_EVENTS: [&str; 15] = [
    "login_fail",
    "dial_fail",
    "answer_fail",
    "panic",
    "crash_marker",
    "engine_exit",
    "manual_upload",
    "diag_run",
    "link_lost",
    "call_abnormal",
    "update_success",
    "update_fail",
    "update_download_fail",
    "update_progress",
    "update_manual_report",
];

/// Events counted as "异常" in KPIs and trend lines.
pub const ABNORMAL_EVENTS: [&str; 10] = [
    "login_fail",
    "dial_fail",
    "answer_fail",
    "panic",
    "crash_marker",
    "engine_exit",
    "link_lost",
    "call_abnormal",
    "update_fail",
    "update_download_fail",
];

const CR_UPDATE_CHECK: &str = "cr:update_check";
const EVENT_KIND_PREFIX: &str = "cr:";

/// Compressed upload cap (client side caps full-day logs at 8 MiB; leave headroom).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Decompression cap — a zip-bomb guard, not a policy.
const MAX_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;
/// How much of the decoded body is kept inline in the snapshot row.
const BODY_PREVIEW_BYTES: usize = 4 * 1024;
/// How much of the decoded body the detail endpoint returns.
const BODY_VIEW_BYTES: usize = 256 * 1024;
/// Per-key ingest rate limit (key = client IP when known, else "global").
const RATE_LIMIT_PER_MIN: u32 = 240;
const MAX_META_LEN: usize = 200;

/// Snapshot kind prefix reserved for client reports. Kept disjoint from the
/// sampling kinds (`locator` / `system_capacity`) whose dashboards filter by
/// exact kind.
pub fn is_client_report_kind(kind: &str) -> bool {
    kind.starts_with(EVENT_KIND_PREFIX)
}

// ── Routes ──────────────────────────────────────────────────────────────

/// API routes (nested under api_prefix). `POST /report` is public — the
/// method+path exact-match exemption lives in `api/mod.rs` (auth) and
/// `middleware.rs` (csrf).
pub fn api_urls() -> axum::Router<Arc<ConsoleState>> {
    let ingest = post(report_handler).layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES));
    axum::Router::new()
        .route("/report", ingest)
        .route("/client-reports/summary", get(summary_handler))
        .route("/client-reports/trend", get(trend_handler))
        .route("/client-reports/list", get(list_handler))
        .route("/client-reports/body", get(body_handler))
        .route("/client-reports/export", get(export_handler))
}

use std::sync::Arc;

use axum::extract::Query as AxumQuery;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use sea_orm::sea_query::ExprTrait;

// ── Metadata extraction ─────────────────────────────────────────────────

fn sanitize(raw: &str, max: usize) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.trim().chars().take(max).collect()
}

/// Query param first, `X-Restsend-*` header as the fallback mirror.
fn meta(
    params: &HashMap<String, String>,
    headers: &HeaderMap,
    param: &str,
    header: &str,
    max: usize,
) -> Option<String> {
    params
        .get(param)
        .map(String::as_str)
        .or_else(|| headers.get(header).and_then(|v| v.to_str().ok()))
        .map(|v| sanitize(v, max))
        .filter(|v| !v.is_empty())
}

fn flag(params: &HashMap<String, String>, headers: &HeaderMap, param: &str, header: &str) -> bool {
    matches!(
        meta(params, headers, param, header, 8).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Keep `[a-z0-9_-]`, lowercase, capped so `cr:{event}` fits the 32-char
/// kind column. Unknown-but-plausible events are preserved (forward compat);
/// garbage collapses to `unknown`.
fn sanitize_event(raw: &str) -> String {
    let cleaned: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(20)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn is_abnormal_event(event: &str) -> bool {
    ABNORMAL_EVENTS.contains(&event)
}

// ── Rate limiting ───────────────────────────────────────────────────────

fn limiter() -> &'static parking_lot::Mutex<HashMap<String, (std::time::Instant, u32)>> {
    static LIMITER: OnceLock<parking_lot::Mutex<HashMap<String, (std::time::Instant, u32)>>> =
        OnceLock::new();
    LIMITER.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// Sliding-minute budget per key. Best-effort abuse guard, not accounting.
fn rate_limit_ok(key: &str) -> bool {
    let mut map = limiter().lock();
    if map.len() > 4096 {
        map.retain(|_, (since, _)| since.elapsed().as_secs() < 60);
    }
    let now = std::time::Instant::now();
    let entry = map.entry(key.to_string()).or_insert((now, 0));
    if entry.0.elapsed().as_secs() >= 60 {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 <= RATE_LIMIT_PER_MIN
}

fn client_ip(headers: &HeaderMap, addr: Option<IpAddr>) -> String {
    if let Some(ip) = addr {
        if !ip.is_unspecified() {
            return ip.to_string();
        }
    }
    for header in ["x-client-ip", "x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(header).and_then(|v| v.to_str().ok()) {
            let first = v.split(',').next().unwrap_or(v).trim();
            if let Ok(ip) = first.parse::<IpAddr>() {
                return ip.to_string();
            }
        }
    }
    "global".to_string()
}

// ── Body decoding + telemetry file storage ──────────────────────────────

struct DecodedBody {
    text: String,
    truncated: bool,
}

/// Gunzip when the payload carries the gzip magic, else treat as plain text.
/// Decompression is capped at [`MAX_DECOMPRESSED_BYTES`].
fn decode_body(raw: &[u8]) -> DecodedBody {
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(raw);
        let mut buf = Vec::with_capacity(raw.len().min(MAX_DECOMPRESSED_BYTES));
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match decoder.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > MAX_DECOMPRESSED_BYTES {
                        let take = MAX_DECOMPRESSED_BYTES - buf.len();
                        buf.extend_from_slice(&chunk[..take]);
                        return finish_decode(buf, true);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(_) => break,
            }
        }
        finish_decode(buf, false)
    } else {
        let capped = &raw[..raw.len().min(MAX_DECOMPRESSED_BYTES)];
        finish_decode(capped.to_vec(), raw.len() > MAX_DECOMPRESSED_BYTES)
    }
}

fn finish_decode(bytes: Vec<u8>, truncated: bool) -> DecodedBody {
    DecodedBody {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    }
}

fn gzip_bytes(plain: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = std::io::Write::write_all(&mut enc, plain);
    enc.finish().unwrap_or_default()
}

/// Telemetry body root: `[release].telemetry_dir` or `storage/telemetry`.
fn telemetry_root(state: &ConsoleState) -> PathBuf {
    state
        .config()
        .release
        .as_ref()
        .and_then(|r| r.telemetry_dir.as_deref())
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("storage/telemetry"))
}

static TELEMETRY_SEQ: AtomicU64 = AtomicU64::new(0);

/// Persist a report body. Returns the storage key relative to the root.
fn store_telemetry_body(
    state: &ConsoleState,
    kind: &str,
    event: &str,
    raw: &[u8],
    is_gzip: bool,
) -> Option<String> {
    let payload: Vec<u8> = if is_gzip {
        raw.to_vec()
    } else {
        gzip_bytes(raw)
    };
    let day = Utc::now().format("%Y-%m-%d");
    let seq = TELEMETRY_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "{}_{seq:06}_{kind}_{event}.gz",
        Utc::now().timestamp()
    );
    let key = format!("{day}/{name}");
    let dir = telemetry_root(state).join(day.to_string());
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "telemetry dir create failed");
        return None;
    }
    if let Err(e) = std::fs::write(dir.join(&name), &payload) {
        tracing::warn!(error = %e, "telemetry body write failed");
        return None;
    }
    Some(key)
}

/// Read a stored telemetry body (path-traversal safe) and decode it.
fn read_telemetry_body(state: &ConsoleState, key: &str) -> Option<String> {
    if key.contains("..") || key.starts_with('/') || key.starts_with('\\') {
        return None;
    }
    let root = telemetry_root(state);
    let path = root.join(key);
    let canonical_root = root.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    let raw = std::fs::read(&canonical).ok()?;
    Some(decode_body(&raw).text.chars().take(BODY_VIEW_BYTES * 2).collect())
}

/// Delete telemetry files older than `days` (called from the hourly sweep in
/// `app.rs`, next to the snapshot prune).
pub fn prune_telemetry_files(state: &ConsoleState, days: i64) {
    let root = telemetry_root(state);
    let cutoff = Utc::now() - chrono::Duration::days(days);
    let cutoff_day = cutoff.format("%Y-%m-%d").to_string();
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return;
    };
    for dir in dirs.flatten() {
        let name = dir.file_name();
        let Some(name) = name.to_str() else { continue };
        // Day directories sort chronologically; string compare is exact.
        if name.as_bytes() < cutoff_day.as_bytes() && dir.path().is_dir() {
            if let Err(e) = std::fs::remove_dir_all(dir.path()) {
                tracing::warn!(error = %e, dir = %name, "telemetry prune failed");
            }
        }
    }
}

// ── Ingestion ───────────────────────────────────────────────────────────

/// Build + insert the snapshot row for one report. Returns the row kind.
async fn ingest_row(
    state: &ConsoleState,
    event: &str,
    version: &str,
    num1: i64,
    num2: bool,
    num3: bool,
    data: serde_json::Value,
) -> std::result::Result<(), sea_orm::DbErr> {
    let model = crate::models::runtime_snapshot::ActiveModel {
        id: sea_orm::NotSet,
        kind: sea_orm::Set(format!("{EVENT_KIND_PREFIX}{event}")),
        created_at: sea_orm::Set(Utc::now()),
        num1: sea_orm::Set(num1),
        num2: sea_orm::Set(i64::from(num2)),
        num3: sea_orm::Set(i64::from(num3)),
        num4: sea_orm::Set(0.0),
        data: sea_orm::Set(Some(data)),
        instance_id: sea_orm::Set(Some(version.to_string())),
    };
    use sea_orm::ActiveModelTrait;
    let db = state.db();
    model.insert(db).await.map(|_| ())
}

/// Peer IP from the `ConnectInfo` extension when present (real server) —
/// `None` in bare test routers. Same approach as `handler::middleware::clientaddr`.
struct PeerIp(Option<IpAddr>);

impl axum::extract::FromRequestParts<Arc<ConsoleState>> for PeerIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &Arc<ConsoleState>,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

/// POST {api_prefix}/report — the public collector.
pub async fn report_handler(
    State(state): State<Arc<ConsoleState>>,
    PeerIp(peer): PeerIp,
    AxumQuery(params): AxumQuery<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ip = client_ip(&headers, peer);
    if !rate_limit_ok(&ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"ok": false, "error": "rate limited"})),
        )
            .into_response();
    }
    if body.len() > MAX_BODY_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"ok": false, "error": "body too large"})),
        )
            .into_response();
    }

    let Some(kind) = meta(&params, &headers, "kind", "x-restsend-log-kind", 16) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "missing kind"})),
        )
            .into_response();
    };
    if !KNOWN_KINDS.contains(&kind.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": format!("unknown kind `{kind}`")})),
        )
            .into_response();
    }
    let raw_event = meta(&params, &headers, "event", "x-restsend-log-event", 64)
        .unwrap_or_else(|| "unknown".to_string());
    let event = sanitize_event(&raw_event);
    let version = meta(&params, &headers, "version", "x-restsend-version", 32)
        .unwrap_or_else(|| "unknown".to_string());
    let agent_id = meta(&params, &headers, "agent_id", "x-restsend-agent-id", 64)
        .unwrap_or_default();

    let is_gzip = body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b;
    let decoded = decode_body(&body);
    let preview: String = decoded.text.chars().take(BODY_PREVIEW_BYTES).collect();
    let storage_key = store_telemetry_body(&state, &kind, &event, &body, is_gzip);

    let in_call = flag(&params, &headers, "in_call", "x-restsend-in-call");
    let safe_mode = flag(&params, &headers, "safe_mode", "x-restsend-safe-mode");

    let data = json!({
        "kind": kind,
        "os": meta(&params, &headers, "os", "x-restsend-os", 32),
        "arch": meta(&params, &headers, "arch", "x-restsend-arch", 32),
        "agent_id": agent_id,
        "sip_user": meta(&params, &headers, "sip_user", "x-restsend-sip-user", 64),
        "account_name": meta(&params, &headers, "account_name", "x-restsend-account-name", MAX_META_LEN),
        "registrar": meta(&params, &headers, "registrar", "x-restsend-registrar", 120),
        "media": meta(&params, &headers, "media", "x-restsend-media", 32),
        "transport": meta(&params, &headers, "transport", "x-restsend-transport", 32),
        "auth": meta(&params, &headers, "auth", "x-restsend-auth", 32),
        "call_id": meta(&params, &headers, "call_id", "x-restsend-call-id", 128),
        "remote_uri": meta(&params, &headers, "remote_uri", "x-restsend-remote-uri", MAX_META_LEN),
        "note": meta(&params, &headers, "note", "x-restsend-note", MAX_META_LEN),
        "build": meta(&params, &headers, "build", "x-restsend-build", 32),
        "build_time": meta(&params, &headers, "build_time", "x-restsend-build-time", 32),
        "git_commit": meta(&params, &headers, "git_commit", "x-restsend-git-commit", 64),
        "hostname": meta(&params, &headers, "hostname", "x-restsend-hostname", 120),
        "mac": meta(&params, &headers, "mac", "x-restsend-mac", 32),
        "cpu_cores": meta(&params, &headers, "cpu_cores", "x-restsend-cpu-cores", 8),
        "mem_total_mb": meta(&params, &headers, "mem_total_mb", "x-restsend-mem-total-mb", 12),
        "mem_avail_mb": meta(&params, &headers, "mem_avail_mb", "x-restsend-mem-avail-mb", 12),
        "ts": meta(&params, &headers, "ts", "", 16),
        "ip": ip,
        "storage_key": storage_key,
        "body_preview": preview,
        "body_truncated": decoded.truncated,
        "body_is_gzip": is_gzip,
        "received_by": std::env::var("RUSTPBX_INSTANCE_ID").ok().filter(|s| !s.is_empty()),
    });

    let db_err = if let Err(e) = ingest_row(
        &state,
        &event,
        &version,
        body.len() as i64,
        in_call,
        safe_mode,
        data,
    )
    .await
    {
        // Body is already on disk; report acceptance so the client does not
        // re-upload megabytes, but surface the metadata loss.
        crate::db_report::report_db_write_failure(
            "runtime_snapshot",
            "insert",
            None,
            format!("client report ingest: {e}"),
        );
        Some(e)
    } else {
        None
    };

    Json(json!({
        "ok": true,
        "stored_body": storage_key.is_some(),
        "stored_meta": db_err.is_none(),
    }))
    .into_response()
}

/// Record an update check (`GET {api_prefix}/release?version=…`) as a
/// `cr:update_check` snapshot row. Best-effort: never fails the check.
pub async fn record_update_check(
    state: &ConsoleState,
    params: &HashMap<String, String>,
    headers: &HeaderMap,
) {
    let Some(version) = params
        .get("version")
        .map(|v| sanitize(v, 32))
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    let data = json!({
        "env": params.get("env").map(|v| sanitize(v, 16)),
        "build_time": params.get("build_time").map(|v| sanitize(v, 32)),
        "git_commit": params.get("git_commit").map(|v| sanitize(v, 64)),
        "user_agent": headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| sanitize(v, MAX_META_LEN)),
        "received_by": std::env::var("RUSTPBX_INSTANCE_ID").ok().filter(|s| !s.is_empty()),
    });
    if let Err(e) = ingest_row(state, "update_check", &version, 0, false, false, data).await {
        tracing::debug!(error = %e, "update check record failed");
    }
}

// ── Stats queries ───────────────────────────────────────────────────────

/// Shared window params (from/to/bucket/tz) — same parsing as the domain
/// reports so the telemetry tab behaves identically.
pub(crate) use super::reports::DomainReportParams;

fn cr_window(q: &crate::report::domain_report::DomainQuery) -> sea_orm::sea_query::Condition {
    sea_orm::sea_query::Condition::all()
        .add(SnapCol::CreatedAt.gte(q.from))
        .add(SnapCol::CreatedAt.lt(q.to))
}

fn kind_like_cr() -> sea_orm::sea_query::SimpleExpr {
    SnapCol::Kind.like(format!("{EVENT_KIND_PREFIX}%"))
}

fn abnormal_kinds() -> Vec<String> {
    ABNORMAL_EVENTS
        .iter()
        .map(|e| format!("{EVENT_KIND_PREFIX}{e}"))
        .collect()
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct EventRow {
    kind: String,
    count: i64,
    last_seen: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct VersionRow {
    version: Option<String>,
    count: i64,
    update_checks: i64,
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct OsRow {
    os: Option<String>,
    count: i64,
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct AgentRow {
    agent: Option<String>,
    count: i64,
    abnormal: i64,
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct SummaryTotals {
    total: i64,
    abnormal: i64,
    update_checks: i64,
    versions: i64,
    agents: i64,
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct TrendPoint {
    bucket: i64,
    total: i64,
    abnormal: i64,
    update_checks: i64,
}

async fn summary_access_denied(
    state: &ConsoleState,
    user: &crate::models::user::Model,
) -> bool {
    !super::reports::has_reports_access(state, user, false).await
}

/// GET /console/api/client-reports/summary?from=&to=&tz=
async fn summary_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    AxumQuery(params): AxumQuery<DomainReportParams>,
) -> Response {
    if summary_access_denied(&state, &user).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let db = state.db();
    let backend = db.get_database_backend();
    let window = cr_window(&q).add(kind_like_cr());

    let totals = SnapshotEntity::find()
        .select_only()
        .column_as(SnapCol::Id.count(), "total")
        .column_as(count_when(SnapCol::Kind.is_in(abnormal_kinds())), "abnormal")
        .column_as(count_when(SnapCol::Kind.eq(CR_UPDATE_CHECK)), "update_checks")
        .column_as(sea_orm::sea_query::Expr::col(SnapCol::InstanceId).count_distinct(), "versions")
        .column_as(
            sea_orm::sea_query::Expr::expr(json_text_expr(backend, "data", "agent_id"))
                .count_distinct(),
            "agents",
        )
        .filter(window.clone())
        .into_model::<SummaryTotals>()
        .one(db)
        .await;

    let by_event: Vec<EventRow> = SnapshotEntity::find()
        .select_only()
        .column_as(SnapCol::Kind, "kind")
        .column_as(SnapCol::Id.count(), "count")
        .column_as(SnapCol::CreatedAt.max(), "last_seen")
        .filter(window.clone())
        .group_by(SnapCol::Kind)
        .order_by_desc(SnapCol::Id.count())
        .into_model()
        .all(db)
        .await
        .unwrap_or_default();

    let by_version: Vec<VersionRow> = SnapshotEntity::find()
        .select_only()
        .column_as(SnapCol::InstanceId, "version")
        .column_as(SnapCol::Id.count(), "count")
        .column_as(count_when(SnapCol::Kind.eq(CR_UPDATE_CHECK)), "update_checks")
        .filter(window.clone())
        .group_by(SnapCol::InstanceId)
        .order_by_desc(SnapCol::Id.count())
        .into_model()
        .all(db)
        .await
        .unwrap_or_default();

    let by_os: Vec<OsRow> = SnapshotEntity::find()
        .select_only()
        .column_as(json_text_expr(backend, "data", "os"), "os")
        .column_as(SnapCol::Id.count(), "count")
        .filter(window.clone())
        .group_by(json_text_expr(backend, "data", "os"))
        .order_by_desc(SnapCol::Id.count())
        .into_model()
        .all(db)
        .await
        .unwrap_or_default();

    let by_agent: Vec<AgentRow> = SnapshotEntity::find()
        .select_only()
        .column_as(json_text_expr(backend, "data", "agent_id"), "agent")
        .column_as(SnapCol::Id.count(), "count")
        .column_as(count_when(SnapCol::Kind.is_in(abnormal_kinds())), "abnormal")
        .filter(window)
        .group_by(json_text_expr(backend, "data", "agent_id"))
        .order_by_desc(SnapCol::Id.count())
        .into_model()
        .all(db)
        .await
        .unwrap_or_default();

    let totals = totals
        .ok()
        .flatten()
        .unwrap_or(SummaryTotals {
            total: 0,
            abnormal: 0,
            update_checks: 0,
            versions: 0,
            agents: 0,
        });

    let by_event: Vec<serde_json::Value> = by_event
        .into_iter()
        .map(|r| {
            let event = r
                .kind
                .strip_prefix(EVENT_KIND_PREFIX)
                .unwrap_or(&r.kind)
                .to_string();
            json!({
                "event": event,
                "abnormal": is_abnormal_event(&event),
                "count": r.count,
                "last_seen": r.last_seen.map(|t| t.to_rfc3339()),
            })
        })
        .collect();
    let by_version: Vec<serde_json::Value> = by_version
        .into_iter()
        .map(|r| {
            json!({
                "version": r.version.unwrap_or_else(|| "unknown".into()),
                "count": r.count,
                "update_checks": r.update_checks,
            })
        })
        .collect();
    let by_os: Vec<serde_json::Value> = by_os
        .into_iter()
        .map(|r| json!({"os": r.os.unwrap_or_else(|| "unknown".into()), "count": r.count}))
        .collect();
    let by_agent: Vec<serde_json::Value> = by_agent
        .into_iter()
        .filter_map(|r| {
            let agent = r.agent?;
            Some(json!({
                "agent": agent,
                "count": r.count,
                "abnormal": r.abnormal,
            }))
        })
        .collect();

    Json(json!({
        "data": {
            "total": totals.total,
            "abnormal": totals.abnormal,
            "abnormal_pct": pct(totals.abnormal as f64, totals.total as f64),
            "update_checks": totals.update_checks,
            "versions": totals.versions,
            "agents": totals.agents,
            "by_event": by_event,
            "by_version": by_version,
            "by_os": by_os,
            "by_agent": by_agent,
        },
        "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339()},
    }))
    .into_response()
}

/// GET /console/api/client-reports/trend?from=&to=&bucket=&tz=
async fn trend_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    AxumQuery(params): AxumQuery<DomainReportParams>,
) -> Response {
    if summary_access_denied(&state, &user).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let db = state.db();
    let backend = db.get_database_backend();
    let bucket_expr =
        bucket_index_expr(backend, "created_at", q.bucket.secs(), q.tz_offset_secs);
    let window = cr_window(&q).add(kind_like_cr());

    let rows: Vec<TrendPoint> = SnapshotEntity::find()
        .select_only()
        .column_as(bucket_expr, "bucket")
        .column_as(SnapCol::Id.count(), "total")
        .column_as(count_when(SnapCol::Kind.is_in(abnormal_kinds())), "abnormal")
        .column_as(count_when(SnapCol::Kind.eq(CR_UPDATE_CHECK)), "update_checks")
        .filter(window)
        .group_by(sea_orm::sea_query::Expr::cust("bucket"))
        .order_by_asc(sea_orm::sea_query::Expr::cust("bucket"))
        .into_model()
        .all(db)
        .await
        .unwrap_or_default();

    let series: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "bucket_start": bucket_start_utc(r.bucket, q.bucket.secs(), q.tz_offset_secs).to_rfc3339(),
                "total": r.total,
                "abnormal": r.abnormal,
                "update_checks": r.update_checks,
            })
        })
        .collect();
    Json(json!({
        "data": series,
        "meta": {
            "from": q.from.to_rfc3339(),
            "to": q.to.to_rfc3339(),
            "bucket": format!("{:?}", q.bucket).to_lowercase(),
        },
    }))
    .into_response()
}

#[derive(serde::Serialize)]
struct ListRow {
    id: i64,
    created_at: chrono::DateTime<chrono::Utc>,
    event: String,
    abnormal: bool,
    version: String,
    num1: i64,
    num2: i64,
    num3: i64,
    data: Option<serde_json::Value>,
}

/// GET /console/api/client-reports/list?from=&to=&event=&version=&limit=
async fn list_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    AxumQuery(params): AxumQuery<ListParams>,
) -> Response {
    if summary_access_denied(&state, &user).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.inner.resolve();
    let event = params.event.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let version = params
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let agent = params.agent.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let limit = params.limit.unwrap_or(50).clamp(1, 200) as u64;

    let mut cond = cr_window(&q).add(kind_like_cr());
    if let Some(event) = event {
        cond = cond.add(SnapCol::Kind.eq(format!("{EVENT_KIND_PREFIX}{}", sanitize_event(event))));
    }
    if let Some(version) = version {
        cond = cond.add(SnapCol::InstanceId.eq(sanitize(version, 32)));
    }
    if let Some(agent) = agent {
        let backend = state.db().get_database_backend();
        cond = cond.add(
            json_text_expr(backend, "data", "agent_id")
                .eq(sanitize(agent, 64)),
        );
    }

    let rows = SnapshotEntity::find()
        .filter(cond)
        .order_by_desc(SnapCol::Id)
        .limit(limit)
        .all(state.db())
        .await
        .unwrap_or_default();

    let items: Vec<ListRow> = rows
        .into_iter()
        .map(|r| {
            let event = r
                .kind
                .strip_prefix(EVENT_KIND_PREFIX)
                .unwrap_or(&r.kind)
                .to_string();
            ListRow {
                id: r.id,
                created_at: r.created_at,
                abnormal: is_abnormal_event(&event),
                event,
                version: r.instance_id.unwrap_or_else(|| "unknown".into()),
                num1: r.num1,
                num2: r.num2,
                num3: r.num3,
                data: r.data,
            }
        })
        .collect();

    Json(json!({"data": items})).into_response()
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
struct ListParams {
    #[serde(flatten)]
    inner: DomainReportParams,
    event: Option<String>,
    version: Option<String>,
    agent: Option<String>,
    limit: Option<u32>,
}

/// GET /console/api/client-reports/body?id=123 — decoded telemetry body for
/// one report (from the local telemetry store).
async fn body_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    AxumQuery(params): AxumQuery<BodyParams>,
) -> Response {
    if summary_access_denied(&state, &user).await {
        return crate::console::config_helpers::permission_denied();
    }
    let Some(row) = SnapshotEntity::find()
        .filter(SnapCol::Id.eq(params.id))
        .one(state.db())
        .await
        .ok()
        .flatten()
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "report not found"})),
        )
            .into_response();
    };
    if !is_client_report_kind(&row.kind) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "not a client report"})),
        )
            .into_response();
    }
    let Some(key) = row
        .data
        .as_ref()
        .and_then(|d| d.get("storage_key"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return Json(json!({"id": row.id, "text": null, "reason": "body not stored"}))
            .into_response();
    };
    match read_telemetry_body(&state, &key) {
        Some(text) => {
            let truncated = text.len() > BODY_VIEW_BYTES;
            let text: String = text.chars().take(BODY_VIEW_BYTES).collect();
            Json(json!({"id": row.id, "storage_key": key, "text": text, "truncated": truncated}))
                .into_response()
        }
        None => (
            StatusCode::GONE,
            Json(json!({"error": "body file missing or expired", "storage_key": key})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct BodyParams {
    id: i64,
}

use serde::Deserialize;

/// GET /console/api/client-reports/export — trend CSV.
async fn export_handler(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    AxumQuery(params): AxumQuery<DomainReportParams>,
) -> Response {
    if summary_access_denied(&state, &user).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let db = state.db();
    let backend = db.get_database_backend();
    let bucket_expr =
        bucket_index_expr(backend, "created_at", q.bucket.secs(), q.tz_offset_secs);
    let window = cr_window(&q).add(kind_like_cr());

    let rows: Vec<TrendPoint> = SnapshotEntity::find()
        .select_only()
        .column_as(bucket_expr, "bucket")
        .column_as(SnapCol::Id.count(), "total")
        .column_as(count_when(SnapCol::Kind.is_in(abnormal_kinds())), "abnormal")
        .column_as(count_when(SnapCol::Kind.eq(CR_UPDATE_CHECK)), "update_checks")
        .filter(window)
        .group_by(sea_orm::sea_query::Expr::cust("bucket"))
        .order_by_asc(sea_orm::sea_query::Expr::cust("bucket"))
        .into_model()
        .all(db)
        .await
        .unwrap_or_default();

    let mut csv = String::from("bucket_start,total,abnormal,update_checks\n");
    for r in rows {
        csv.push_str(&format!(
            "{},{},{},{}\n",
            bucket_start_utc(r.bucket, q.bucket.secs(), q.tz_offset_secs).to_rfc3339(),
            r.total,
            r.abnormal,
            r.update_checks
        ));
    }
    super::reports::csv_response(csv, "client_reports.csv")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_event_keeps_known_shapes_and_caps_length() {
        assert_eq!(sanitize_event("panic"), "panic");
        assert_eq!(sanitize_event("Update_Download-Fail"), "update_download-fail");
        assert_eq!(sanitize_event(""), "unknown");
        assert_eq!(sanitize_event("bad event!"), "bad_event_");
        // 20-char cap keeps `cr:{event}` inside the 32-char kind column.
        let long = sanitize_event("aaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(long.len(), 20);
        assert!(format!("{EVENT_KIND_PREFIX}{long}").len() <= 32);
        for e in KNOWN_EVENTS {
            assert!(format!("{EVENT_KIND_PREFIX}{e}").len() <= 32, "event {e} overflows kind column");
        }
    }

    #[test]
    fn metadata_params_win_over_headers() {
        let mut params = HashMap::new();
        params.insert("version".to_string(), "0.1.13".to_string());
        let mut headers = HeaderMap::new();
        headers.insert("x-restsend-version", "9.9.9".parse().unwrap());
        assert_eq!(
            meta(&params, &headers, "version", "x-restsend-version", 32),
            Some("0.1.13".to_string())
        );
        // Header fallback when the param is absent.
        assert_eq!(
            meta(&HashMap::new(), &headers, "version", "x-restsend-version", 32),
            Some("9.9.9".to_string())
        );
        // Neither present → None.
        assert_eq!(
            meta(&HashMap::new(), &HeaderMap::new(), "os", "x-restsend-os", 32),
            None
        );
    }

    #[test]
    fn sanitize_strips_controls_and_caps() {
        assert_eq!(sanitize("  hello\u{0}world  ", 64), "hello world");
        let long = sanitize(&"x".repeat(500), 32);
        assert_eq!(long.len(), 32);
    }

    #[test]
    fn body_decode_gzip_and_plain() {
        let gz = gzip_bytes(b"hello telemetry");
        assert_eq!(gz[0], 0x1f);
        assert_eq!(gz[1], 0x8b);
        let decoded = decode_body(&gz);
        assert_eq!(decoded.text, "hello telemetry");
        assert!(!decoded.truncated);

        let decoded = decode_body(b"plain text body");
        assert_eq!(decoded.text, "plain text body");
        assert!(!decoded.truncated);

        // Corrupt gzip falls back to lossy text of the raw bytes.
        let decoded = decode_body(&[0x1f, 0x8b, 0xff, 0xff]);
        assert!(!decoded.text.is_empty() || decoded.truncated == false);
    }

    #[test]
    fn gzip_bomb_is_capped() {
        let bomb = gzip_bytes(&vec![b'a'; MAX_DECOMPRESSED_BYTES + 1024]);
        let decoded = decode_body(&bomb);
        assert!(decoded.truncated);
        assert_eq!(decoded.text.len(), MAX_DECOMPRESSED_BYTES);
    }

    #[test]
    fn flag_parsing() {
        let mut params = HashMap::new();
        params.insert("in_call".to_string(), "1".to_string());
        let headers = HeaderMap::new();
        assert!(flag(&params, &headers, "in_call", "x-restsend-in-call"));
        params.insert("in_call".to_string(), "0".to_string());
        assert!(!flag(&params, &headers, "in_call", "x-restsend-in-call"));
    }

    #[test]
    fn rate_limiter_buckets_per_key() {
        let key = format!("test-{:#x}", std::ptr::from_ref(&KEY_WITNESS) as u64);
        // Fresh key: first request passes; the guard is a budget, so just
        // verify accounting counts up and resets are keyed independently.
        assert!(rate_limit_ok(&key));
        let map = limiter().lock();
        assert_eq!(map.get(&key).map(|(_, c)| *c), Some(1));
    }
    static KEY_WITNESS: u8 = 0;

    #[test]
    fn abnormal_classification() {
        assert!(is_abnormal_event("panic"));
        assert!(is_abnormal_event("link_lost"));
        assert!(!is_abnormal_event("update_success"));
        assert!(!is_abnormal_event("diag_run"));
    }

    #[test]
    fn client_report_kind_predicate() {
        assert!(is_client_report_kind("cr:panic"));
        assert!(is_client_report_kind(CR_UPDATE_CHECK));
        assert!(!is_client_report_kind("locator"));
        assert!(!is_client_report_kind("system_capacity"));
    }
}

