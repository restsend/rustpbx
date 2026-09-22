use crate::console::middleware::AuthRequired;
use crate::console::ConsoleState;
use axum::response::IntoResponse;
use axum::{extract::State, http::HeaderMap, response::Response};
use chrono::Timelike;
use serde_json::json;
use std::sync::Arc;

/// Render the unified reports page (domain tabs backed by
/// `/console/api/reports/v2/*`).
pub async fn reports_page(
    State(state): State<Arc<ConsoleState>>,
    headers: HeaderMap,
    AuthRequired(user): AuthRequired,
) -> Response {
    let current_user = state.build_current_user_ctx(&user).await;
    state.render_with_headers(
        "console/reports.html",
        json!({
            "nav_active": "reports",
            "current_user": current_user,
            // The voicemail addon is commerce-gated — its tables (and data)
            // only exist when the feature build is on, so the tab is hidden
            // otherwise.
            "features": {
                "voicemail": cfg!(feature = "addon-voicemail"),
            },
        }),
        &headers,
    )
}

pub fn urls() -> axum::Router<Arc<ConsoleState>> {
    axum::Router::new().route("/reports", axum::routing::get(reports_page))
}

// ════════════════════════════════════════════════════════════════════
// Domain report API — /console/api/reports/v2/{domain}
// ════════════════════════════════════════════════════════════════════

use axum::routing::get;
use axum::{Json, Router};

/// Shared report window parameters (from/to/bucket/tz/direction).
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub(crate) struct DomainReportParams {
    pub from: Option<String>,
    pub to: Option<String>,
    pub bucket: Option<String>,
    /// `+08:00` / `08:00` / `8` (east positive hours). Default UTC.
    pub tz: Option<String>,
    /// inbound / outbound / internal (call-domain filter).
    pub direction: Option<String>,
    /// Skill-group filter (CC domain — consumed by the addon's handlers).
    #[allow(dead_code)]
    pub group_id: Option<String>,
    /// Trunk id filter (trunk heat — consumed by the core handler).
    #[allow(dead_code)]
    pub trunk_id: Option<String>,
}

impl DomainReportParams {
    pub(crate) fn tz_offset_secs(&self) -> i64 {
        let Some(ref tz) = self.tz else { return 0 };
        let s = tz.trim().trim_start_matches('+');
        if let Ok(hours) = s.parse::<i64>() {
            return hours.clamp(-12, 14) * 3600;
        }
        if let Some((h, m)) = s.split_once(':')
            && let (Ok(h), Ok(m)) = (h.parse::<i64>(), m.parse::<i64>())
        {
            let sign = if h < 0 { -1 } else { 1 };
            return (h.abs() * 3600 + m * 60) * sign;
        }
        0
    }

    fn parse_bound(s: &str, tz_offset: i64, end_of_day: bool) -> Option<chrono::DateTime<chrono::Utc>> {
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(ts.with_timezone(&chrono::Utc));
        }
        let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
        let time = if end_of_day {
            d.and_hms_opt(23, 59, 59).unwrap()
        } else {
            d.and_hms_opt(0, 0, 0).unwrap()
        };
        Some((time - chrono::Duration::seconds(tz_offset)).and_utc())
    }

    pub(crate) fn resolve(self) -> crate::report::domain_report::DomainQuery {
        let tz = self.tz_offset_secs();
        let now = chrono::Utc::now();
        let to = self
            .to
            .as_deref()
            .and_then(|s| Self::parse_bound(s, tz, true))
            .unwrap_or(now);
        let from = self
            .from
            .as_deref()
            .and_then(|s| Self::parse_bound(s, tz, false))
            .unwrap_or_else(|| {
                let shifted = to - chrono::Duration::seconds(tz);
                let day_start = shifted.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
                day_start + chrono::Duration::seconds(tz)
            });
        crate::report::domain_report::DomainQuery {
            from,
            to,
            bucket: self
                .bucket
                .as_deref()
                .and_then(crate::report::TimeBucket::parse)
                .unwrap_or(crate::report::TimeBucket::Day),
            tz_offset_secs: tz,
            direction: self.direction.filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("any")),
        }
    }
}

/// Unified report access: the dedicated `reports:read` point, the existing
/// CDR read permission, or (CC domains) the addon's `cc_reports:read`.
/// Superusers pass through inside `has_permission`.
pub(crate) async fn has_reports_access(
    state: &ConsoleState,
    user: &crate::models::user::Model,
    cc_domain: bool,
) -> bool {
    if state.has_permission(user, "reports", "read").await {
        return true;
    }
    if state.has_permission(user, "cdr", "read").await {
        return true;
    }
    if cc_domain && state.has_permission(user, "cc_reports", "read").await {
        return true;
    }
    false
}

pub(crate) fn bucket_label(q: &crate::report::domain_report::DomainQuery) -> String {
    format!("{:?}", q.bucket).to_lowercase()
}

// ── call ────────────────────────────────────────────────────────────────

async fn domain_call(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let db = state.db();
    let series = match crate::report::domain_report::call_series(db, &q).await {
        Ok(s) => s,
        Err(e) => return Json(json!({"error": format!("call report failed: {e}")})).into_response(),
    };
    let reasons = crate::report::domain_report::hangup_reason_breakdown(db, &q)
        .await
        .unwrap_or_default();
    let statuses = crate::report::domain_report::sip_status_breakdown(db, &q, 10)
        .await
        .unwrap_or_default();
    let top_callers = crate::report::domain_report::top_numbers(db, &q, 10)
        .await
        .unwrap_or_default();

    // Busy-hour heatmap (weekday × hour-of-day in the caller's tz) computed
    // from the hour-bucket series so the bucket SQL stays dialect-shared.
    let mut heat_query = q.clone();
    heat_query.bucket = crate::report::TimeBucket::Hour;
    let hourly = crate::report::domain_report::call_series(db, &heat_query)
        .await
        .unwrap_or_default();
    let mut heat = [[0i64; 24]; 7];
    for p in &hourly {
        let Ok(t) = chrono::DateTime::parse_from_rfc3339(&p.bucket_start) else {
            continue;
        };
        let local = t + chrono::Duration::seconds(q.tz_offset_secs);
        use chrono::Datelike;
        heat[local.weekday().num_days_from_monday() as usize][local.hour() as usize] += p.total;
    }

    Json(json!({
        "data": {
            "series": series,
            "hangup_reasons": reasons,
            "sip_status": statuses,
            "top_callers": top_callers,
            "heatmap": heat,
        },
        "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339(), "bucket": bucket_label(&q)},
    }))
    .into_response()
}

async fn domain_call_export(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let series = crate::report::domain_report::call_series(state.db(), &q)
        .await
        .unwrap_or_default();
    let mut csv = String::from(
        "bucket_start,total,answered,missed,total_duration_secs,avg_duration_secs,asr_pct\n",
    );
    for p in series {
        csv.push_str(&format!(
            "{},{},{},{},{},{:.2},{:.2}\n",
            p.bucket_start,
            p.total,
            p.answered,
            p.missed,
            p.total_duration_secs,
            p.avg_duration_secs,
            p.asr_pct
        ));
    }
    csv_response(csv, "call_report.csv")
}

// ── trunk ───────────────────────────────────────────────────────────────

async fn domain_trunk(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    match crate::report::domain_report::trunk_report(state.db(), &q).await {
        Ok(rows) => Json(json!({
            "data": rows,
            "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339(), "bucket": bucket_label(&q)},
        }))
        .into_response(),
        Err(e) => Json(json!({"error": format!("trunk report failed: {e}")})).into_response(),
    }
}

async fn domain_trunk_export(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let rows = crate::report::domain_report::trunk_report(state.db(), &q)
        .await
        .unwrap_or_default();
    let mut csv = String::from(
        "trunk_id,name,carrier,offered,answered,missed,total_duration_secs,asr_pct\n",
    );
    for r in rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{:.2}\n",
            r.trunk_id.map(|v| v.to_string()).unwrap_or_default(),
            csv_escape(&r.name),
            csv_escape(r.carrier.as_deref().unwrap_or("")),
            r.offered,
            r.answered,
            r.missed,
            r.total_duration_secs,
            r.asr_pct
        ));
    }
    csv_response(csv, "trunk_report.csv")
}

// ── route ───────────────────────────────────────────────────────────────

async fn domain_route(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    match crate::report::domain_report::route_report(state.db(), &q).await {
        Ok(rows) => Json(json!({
            "data": rows,
            "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339()},
        }))
        .into_response(),
        Err(e) => Json(json!({"error": format!("route report failed: {e}")})).into_response(),
    }
}

async fn domain_trunk_heat(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let trunk_id = params
        .trunk_id
        .clone()
        .filter(|g| !g.is_empty())
        .and_then(|g| g.parse::<i64>().ok());
    let mut q = params.resolve();
    // Heatmaps are always weekday × hour-of-day grids.
    q.bucket = crate::report::TimeBucket::Hour;
    match crate::report::domain_report::trunk_heat(state.db(), &q, trunk_id).await {
        Ok(grids) => Json(json!({
            "data": grids,
            "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339(), "bucket": bucket_label(&q)},
        }))
        .into_response(),
        Err(e) => Json(json!({"error": format!("trunk heat failed: {e}")})).into_response(),
    }
}

async fn domain_route_export(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let rows = crate::report::domain_report::route_report(state.db(), &q)
        .await
        .unwrap_or_default();
    let mut csv =
        String::from("route_id,name,offered,answered,missed,total_duration_secs,asr_pct\n");
    for r in rows {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{:.2}\n",
            r.route_id.map(|v| v.to_string()).unwrap_or_default(),
            csv_escape(&r.name),
            r.offered,
            r.answered,
            r.missed,
            r.total_duration_secs,
            r.asr_pct
        ));
    }
    csv_response(csv, "route_report.csv")
}

// ── voicemail ───────────────────────────────────────────────────────────

async fn domain_voicemail(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    match crate::report::domain_report::voicemail_report(state.db(), &q).await {
        Ok((summary, trend, boxes)) => Json(json!({
            "data": {"summary": summary, "trend": trend, "boxes": boxes},
            "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339(), "bucket": bucket_label(&q)},
        }))
        .into_response(),
        Err(e) => Json(json!({"error": format!("voicemail report failed: {e}")})).into_response(),
    }
}

async fn domain_voicemail_export(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let (_, trend, boxes) = crate::report::domain_report::voicemail_report(state.db(), &q)
        .await
        .unwrap_or_default();
    let mut csv = String::from("section,bucket_or_extension,count,unread\n");
    for t in trend {
        csv.push_str(&format!("trend,{},{},\n", t.bucket_start, t.count));
    }
    for b in boxes {
        csv.push_str(&format!("mailbox,{},{},{}\n", csv_escape(&b.extension), b.total, b.unread));
    }
    csv_response(csv, "voicemail_report.csv")
}

// ── locator ─────────────────────────────────────────────────────────────

async fn locator_live(state: &ConsoleState) -> crate::report::domain_report::LocatorLive {
    let mut live = crate::report::domain_report::LocatorLive::default();
    if let Some(server) = state.sip_server()
        && let Ok(stats) = server.locator.online_stats().await
    {
        live.available = true;
        live.online_locations = stats.online_locations as i64;
        live.online_users = stats.online_users as i64;
        live.webrtc_locations = stats.webrtc_locations as i64;
        live.by_transport = serde_json::to_value(&stats.by_transport).unwrap_or_default();
    }
    live
}

async fn domain_locator(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let trend = crate::report::domain_report::locator_trend(state.db(), &q)
        .await
        .unwrap_or_default();
    let live = locator_live(&state).await;
    Json(json!({
        "data": {"live": live, "trend": trend},
        "meta": {"from": q.from.to_rfc3339(), "to": q.to.to_rfc3339(), "bucket": bucket_label(&q)},
    }))
    .into_response()
}

async fn domain_locator_export(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(user): AuthRequired,
    axum::extract::Query(params): axum::extract::Query<DomainReportParams>,
) -> Response {
    if !has_reports_access(&state, &user, false).await {
        return crate::console::config_helpers::permission_denied();
    }
    let q = params.resolve();
    let trend = crate::report::domain_report::locator_trend(state.db(), &q)
        .await
        .unwrap_or_default();
    let mut csv = String::from("bucket_start,online_locations,online_users,webrtc_locations\n");
    for t in trend {
        csv.push_str(&format!(
            "{},{:.2},{:.2},{:.2}\n",
            t.bucket_start, t.online_locations, t.online_users, t.webrtc_locations
        ));
    }
    csv_response(csv, "locator_report.csv")
}

// ── shared helpers ──────────────────────────────────────────────────────

pub(crate) fn csv_response(csv: String, filename: &str) -> Response {
    let disposition = format!("attachment; filename=\"{filename}\"");
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (axum::http::header::CONTENT_DISPOSITION, disposition.as_str()),
        ],
        csv,
    )
        .into_response()
}

/// RFC4180 escaping shared by domain CSV exports.
pub(crate) fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// API routes (mounted under the console api_prefix).
pub fn api_urls() -> Router<Arc<ConsoleState>> {
    let router = Router::new()
        .route("/reports/v2/call", get(domain_call))
        .route("/reports/v2/call/export", get(domain_call_export))
        .route("/reports/v2/trunk", get(domain_trunk))
        .route("/reports/v2/trunk/export", get(domain_trunk_export))
        .route("/reports/v2/trunk/heat", get(domain_trunk_heat))
        .route("/reports/v2/route", get(domain_route))
        .route("/reports/v2/route/export", get(domain_route_export))
        .route("/reports/v2/voicemail", get(domain_voicemail))
        .route("/reports/v2/voicemail/export", get(domain_voicemail_export))
        .route("/reports/v2/locator", get(domain_locator))
        .route("/reports/v2/locator/export", get(domain_locator_export));
    router
}
