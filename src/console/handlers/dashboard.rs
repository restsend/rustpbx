use crate::console::{ConsoleState, middleware::AuthRequired};
use crate::models::call_record::{Column as CallRecordColumn, Entity as CallRecordEntity};
use anyhow::Result;
use axum::{
    Json,
    extract::{Query, State},
    response::Response,
};
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, FromQueryResult, PaginatorTrait,
    QueryFilter, QuerySelect, sea_query,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::warn;

pub async fn dashboard(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let range = resolve_time_range(None);
    let payload = match build_dashboard_payload(&state, &range).await {
        Ok(payload) => payload,
        Err(err) => {
            warn!(error = %err, "failed to build dashboard payload");
            DashboardPayload::empty(range)
        }
    };

    state.render(
        "console/dashboard.html",
        json!({
            "nav_active": "dashboard",
            "metrics": payload.metrics,
            "call_direction": payload.call_direction,
            "active_calls": payload.active_calls,
            "range": payload.range,
        }),
    )
}

#[derive(Deserialize)]
pub struct DashboardDataQuery {
    range: Option<String>,
}

pub async fn dashboard_data(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
    Query(query): Query<DashboardDataQuery>,
) -> Result<Json<DashboardPayload>, Response> {
    let range = resolve_time_range(query.range.as_deref());
    match build_dashboard_payload(&state, &range).await {
        Ok(payload) => Ok(Json(payload)),
        Err(err) => {
            warn!(error = %err, "failed to build dashboard payload");
            Ok(Json(DashboardPayload::empty(range)))
        }
    }
}

#[derive(Clone, Serialize)]
pub struct DashboardPayload {
    range: DashboardRange,
    metrics: DashboardMetrics,
    call_direction: BTreeMap<String, i64>,
    active_calls: Vec<ActiveCallPreview>,
}

impl DashboardPayload {
    fn empty(range: TimeRange) -> Self {
        let bucket_count = range.bucket_count.max(1);
        let total_seconds = range.duration().num_seconds().max(60);
        let bucket_seconds = (total_seconds as f64 / bucket_count as f64)
            .ceil()
            .max(60.0) as i64;
        let (timeline, labels) = build_timeline_from_buckets(Vec::new(), &range, bucket_seconds);
        Self {
            range: range.descriptor(),
            metrics: DashboardMetrics {
                recent10: RecentMetrics {
                    total: 0,
                    trend: "+0%".to_string(),
                    util: 0,
                    answered: 0,
                    asr: "—".to_string(),
                    ans_util: 0,
                    acd: "0s".to_string(),
                    acd_util: 0,
                    timeline,
                    timeline_labels: labels,
                },
                today: TodayMetrics {
                    acd: "0s".to_string(),
                },
                active: 0,
                capacity: 0,
                active_util: 0,
            },
            call_direction: default_direction_map(),
            active_calls: Vec::new(),
        }
    }
}

#[derive(Clone, Serialize)]
pub struct DashboardRange {
    key: String,
    label: String,
    period_label: String,
    period_label_short: String,
    timeline_title: String,
}

#[derive(Clone, Serialize)]
pub struct DashboardMetrics {
    recent10: RecentMetrics,
    today: TodayMetrics,
    active: u32,
    capacity: u32,
    active_util: u32,
}

#[derive(Clone, Serialize)]
pub struct RecentMetrics {
    total: u32,
    trend: String,
    util: u32,
    answered: u32,
    asr: String,
    ans_util: u32,
    acd: String,
    acd_util: u32,
    timeline: Vec<i64>,
    timeline_labels: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct TodayMetrics {
    acd: String,
}

#[derive(Clone, Serialize)]
pub struct ActiveCallPreview {
    session_id: String,
    caller: String,
    callee: String,
    status: String,
    started_at: String,
    duration: String,
    #[serde(skip_serializing)]
    started_at_ts: DateTime<Utc>,
}

#[derive(Clone)]
struct TimeRange {
    key: String,
    label: String,
    period_label: String,
    period_label_short: String,
    timeline_title: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    previous_start: DateTime<Utc>,
    previous_end: DateTime<Utc>,
    bucket_count: usize,
}

impl TimeRange {
    fn descriptor(&self) -> DashboardRange {
        DashboardRange {
            key: self.key.clone(),
            label: self.label.clone(),
            period_label: self.period_label.clone(),
            period_label_short: self.period_label_short.clone(),
            timeline_title: self.timeline_title.clone(),
        }
    }

    fn duration(&self) -> Duration {
        self.end - self.start
    }
}

#[derive(Debug, FromQueryResult)]
pub struct TimelineBucket {
    bucket: i64,
    count: i64,
}

async fn build_dashboard_payload(
    state: &ConsoleState,
    range: &TimeRange,
) -> Result<DashboardPayload> {
    let db = state.db();

    #[derive(Debug, FromQueryResult)]
    struct RecentStats {
        total: i64,
        answered: Option<i64>,
        total_duration: Option<i64>,
    }

    let recent_stats = CallRecordEntity::find()
        .filter(CallRecordColumn::StartedAt.gte(range.start))
        .filter(CallRecordColumn::StartedAt.lt(range.end))
        .select_only()
        .column_as(CallRecordColumn::Id.count(), "total")
        .column_as(
            sea_query::SimpleExpr::from(sea_query::Func::sum(
                sea_query::CaseStatement::new()
                    .case(
                        CallRecordColumn::Status.is_in(["answered", "completed"]),
                        sea_query::Expr::val(1),
                    )
                    .finally(sea_query::Expr::val(0)),
            ))
            .cast_as(sea_query::Alias::new("SIGNED")),
            "answered",
        )
        .column_as(
            sea_query::SimpleExpr::from(sea_query::Func::sum(
                sea_query::CaseStatement::new()
                    .case(
                        CallRecordColumn::Status.is_in(["answered", "completed"]),
                        CallRecordColumn::DurationSecs.into_expr(),
                    )
                    .finally(sea_query::Expr::val(0)),
            ))
            .cast_as(sea_query::Alias::new("SIGNED")),
            "total_duration",
        )
        .into_model::<RecentStats>()
        .one(db)
        .await?
        .unwrap_or(RecentStats {
            total: 0,
            answered: None,
            total_duration: None,
        });

    let bucket_count = range.bucket_count.max(1);
    let total_seconds = range.duration().num_seconds().max(60);
    let bucket_seconds = (total_seconds as f64 / bucket_count as f64)
        .ceil()
        .max(60.0) as i64;

    let backend = db.get_database_backend();
    let start_timestamp = range.start.timestamp();

    let time_expr = match backend {
        DatabaseBackend::Sqlite => sea_query::Expr::cust(format!(
            "CAST((strftime('%s', started_at) - {}) / {} AS INTEGER)",
            start_timestamp, bucket_seconds
        )),
        DatabaseBackend::MySql => sea_query::Expr::cust(format!(
            "CAST(FLOOR((UNIX_TIMESTAMP(started_at) - {}) / {}) AS SIGNED)",
            start_timestamp, bucket_seconds
        )),
        DatabaseBackend::Postgres => sea_query::Expr::cust(format!(
            "CAST(FLOOR((EXTRACT(EPOCH FROM started_at) - {}) / {}) AS BIGINT)",
            start_timestamp, bucket_seconds
        )),
    };

    let timeline_buckets = CallRecordEntity::find()
        .filter(CallRecordColumn::StartedAt.gte(range.start))
        .filter(CallRecordColumn::StartedAt.lt(range.end))
        .select_only()
        .column_as(time_expr, "bucket")
        .column_as(CallRecordColumn::Id.count(), "count")
        .group_by(sea_query::Expr::col(sea_query::Alias::new("bucket")))
        .into_model::<TimelineBucket>()
        .all(db)
        .await?;

    let previous_count = CallRecordEntity::find()
        .filter(CallRecordColumn::StartedAt.gte(range.previous_start))
        .filter(CallRecordColumn::StartedAt.lt(range.previous_end))
        .count(db)
        .await?;

    let today_start = start_of_day(Utc::now());

    #[derive(Debug, FromQueryResult)]
    struct TodayStats {
        answered_count: Option<i64>,
        total_duration: Option<i64>,
    }

    let today_stats = CallRecordEntity::find()
        .filter(CallRecordColumn::StartedAt.gte(today_start))
        .select_only()
        .column_as(
            sea_query::SimpleExpr::from(sea_query::Func::sum(
                sea_query::CaseStatement::new()
                    .case(
                        CallRecordColumn::Status.is_in(["answered", "completed"]),
                        sea_query::Expr::val(1),
                    )
                    .finally(sea_query::Expr::val(0)),
            ))
            .cast_as(sea_query::Alias::new("SIGNED")),
            "answered_count",
        )
        .column_as(
            sea_query::SimpleExpr::from(sea_query::Func::sum(
                sea_query::CaseStatement::new()
                    .case(
                        CallRecordColumn::Status.is_in(["answered", "completed"]),
                        CallRecordColumn::DurationSecs.into_expr(),
                    )
                    .finally(sea_query::Expr::val(0)),
            ))
            .cast_as(sea_query::Alias::new("SIGNED")),
            "total_duration",
        )
        .into_model::<TodayStats>()
        .one(db)
        .await?
        .unwrap_or(TodayStats {
            answered_count: None,
            total_duration: None,
        });

    #[derive(Debug, FromQueryResult)]
    struct DirectionStat {
        direction: String,
        count: i64,
    }

    let direction_stats = CallRecordEntity::find()
        .filter(CallRecordColumn::StartedAt.gte(range.start))
        .filter(CallRecordColumn::StartedAt.lt(range.end))
        .select_only()
        .column(CallRecordColumn::Direction)
        .column_as(CallRecordColumn::Id.count(), "count")
        .group_by(CallRecordColumn::Direction)
        .into_model::<DirectionStat>()
        .all(db)
        .await?;

    let (active_total, active_preview) = active_call_stats(state, 10).await;

    let capacity = state
        .sip_server()
        .and_then(|server| server.proxy_config.max_concurrency)
        .unwrap_or(0) as u32;

    let total_recent = recent_stats.total as u32;
    let answered_recent = recent_stats.answered.unwrap_or(0) as u32;
    let avg_recent_duration = if answered_recent > 0 {
        Some(recent_stats.total_duration.unwrap_or(0) / answered_recent as i64)
    } else {
        None
    };

    let today_answered = today_stats.answered_count.unwrap_or(0);
    let today_avg_duration = if today_answered > 0 {
        Some(today_stats.total_duration.unwrap_or(0) / today_answered)
    } else {
        None
    };

    let (timeline, timeline_labels) =
        build_timeline_from_buckets(timeline_buckets, range, bucket_seconds);
    let trend = calc_trend_string(total_recent, previous_count as u32);
    let asr_string = if total_recent > 0 {
        format!(
            "{:.0}%",
            (answered_recent as f64 / total_recent as f64 * 100.0).round()
        )
    } else {
        "—".to_string()
    };

    let metrics = DashboardMetrics {
        recent10: RecentMetrics {
            total: total_recent,
            trend,
            util: calc_util(total_recent, capacity),
            answered: answered_recent,
            asr: asr_string,
            ans_util: calc_util(answered_recent, capacity),
            acd: format_duration(avg_recent_duration.unwrap_or(0)),
            acd_util: calc_duration_util(avg_recent_duration.unwrap_or(0)),
            timeline,
            timeline_labels,
        },
        today: TodayMetrics {
            acd: format_duration(today_avg_duration.unwrap_or(0)),
        },
        active: active_total as u32,
        capacity,
        active_util: calc_util(active_total as u32, capacity),
    };

    let mut direction_counts: BTreeMap<String, i64> = BTreeMap::new();
    for stat in direction_stats {
        let label = direction_label(&stat.direction);
        *direction_counts.entry(label).or_insert(0) += stat.count;
    }
    ensure_direction_defaults(&mut direction_counts);

    Ok(DashboardPayload {
        range: range.descriptor(),
        metrics,
        call_direction: direction_counts,
        active_calls: active_preview,
    })
}

fn default_direction_map() -> BTreeMap<String, i64> {
    let mut map = BTreeMap::new();
    ensure_direction_defaults(&mut map);
    map
}

fn build_timeline_from_buckets(
    buckets: Vec<TimelineBucket>,
    range: &TimeRange,
    bucket_seconds: i64,
) -> (Vec<i64>, Vec<String>) {
    let bucket_count = range.bucket_count.max(1);
    let mut series = vec![0i64; bucket_count];

    for b in buckets {
        if b.bucket >= 0 && (b.bucket as usize) < bucket_count {
            series[b.bucket as usize] = b.count;
        } else if bucket_count > 0 && b.bucket >= 0 {
            *series.last_mut().unwrap() += b.count;
        }
    }

    let mut labels = Vec::with_capacity(bucket_count);
    for i in 0..bucket_count {
        let bucket_end = range.start + Duration::seconds(bucket_seconds * (i as i64 + 1));
        let clamped_end = if bucket_end > range.end {
            range.end
        } else {
            bucket_end
        };
        labels.push(format_timeline_label(range, clamped_end));
    }

    (series, labels)
}

fn format_timeline_label(range: &TimeRange, timestamp: DateTime<Utc>) -> String {
    let total_seconds = range.duration().num_seconds();
    if total_seconds <= 3600 {
        timestamp.format("%H:%M").to_string()
    } else if total_seconds <= 172_800 {
        timestamp.format("%d %H:%M").to_string()
    } else {
        timestamp.format("%m-%d").to_string()
    }
}

async fn active_call_stats(state: &ConsoleState, limit: usize) -> (usize, Vec<ActiveCallPreview>) {
    let mut previews = Vec::new();
    let mut active_count = 0;
    // 2. Proxy Calls (including Wholesale)
    if let Ok(guard) = state.sip_server.read() {
        if let Some(server) = guard.as_ref() {
            active_count += server.active_call_registry.count();
            let proxy_calls = server.active_call_registry.list_recent(limit);
            for call in proxy_calls {
                let status = call.status.to_string();
                // Capitalize status
                let status = if !status.is_empty() {
                    let mut c = status.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                } else {
                    status
                };

                let start_time = call.started_at;
                let started_at = start_time.format("%H:%M").to_string();
                let duration_secs = if let Some(answered_at) = call.answered_at {
                    (Utc::now() - answered_at).num_seconds().max(0)
                } else {
                    (Utc::now() - start_time).num_seconds().max(0)
                };

                previews.push(ActiveCallPreview {
                    session_id: call.session_id.clone(),
                    caller: call
                        .caller
                        .clone()
                        .unwrap_or_else(|| call.session_id.clone()),
                    callee: call
                        .callee
                        .clone()
                        .unwrap_or_else(|| call.direction.clone()),
                    status,
                    started_at,
                    duration: format_duration(duration_secs),
                    started_at_ts: start_time,
                });
            }
        }
    }

    // Sort and limit
    previews.sort_by(|a, b| b.started_at_ts.cmp(&a.started_at_ts));
    if previews.len() > limit {
        previews.truncate(limit);
    }

    previews.sort_by(|a, b| b.started_at_ts.cmp(&a.started_at_ts));
    if previews.len() > limit {
        previews.truncate(limit);
    }
    (active_count, previews)
}

fn calc_trend_string(current: u32, previous: u32) -> String {
    if previous == 0 {
        if current == 0 {
            "+0%".to_string()
        } else {
            "+100%".to_string()
        }
    } else {
        let diff = current as f64 - previous as f64;
        let percent = (diff / previous as f64) * 100.0;
        if percent.is_finite() {
            format!("{:+.0}%", percent.round())
        } else {
            "+0%".to_string()
        }
    }
}

fn calc_util(value: u32, capacity: u32) -> u32 {
    if capacity == 0 {
        0
    } else {
        ((value.min(capacity) as f64 / capacity as f64) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u32
    }
}

fn calc_duration_util(seconds: i64) -> u32 {
    if seconds <= 0 {
        return 0;
    }
    let reference = 5 * 60;
    ((seconds.min(reference) as f64 / reference as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u32
}

fn format_duration(seconds: i64) -> String {
    if seconds <= 0 {
        return "0s".to_string();
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        if minutes > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}h", hours)
        }
    } else if minutes > 0 {
        if secs > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{}m", minutes)
        }
    } else {
        format!("{}s", secs)
    }
}

fn direction_label(direction: &str) -> String {
    let normalized = direction.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "inbound" => "Inbound".to_string(),
        "outbound" => "Outbound".to_string(),
        "internal" => "Internal".to_string(),
        other if other.is_empty() => "Unknown".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => "Unknown".to_string(),
            }
        }
    }
}

fn ensure_direction_defaults(map: &mut BTreeMap<String, i64>) {
    for key in ["Inbound", "Outbound", "Internal"] {
        map.entry(key.to_string()).or_insert(0);
    }
}

fn resolve_time_range(input: Option<&str>) -> TimeRange {
    let now = Utc::now();
    match input.unwrap_or("10m") {
        "today" => {
            let start = start_of_day(now);
            let previous_start = start - Duration::days(1);
            TimeRange {
                key: "today".to_string(),
                label: "Today".to_string(),
                period_label: "today".to_string(),
                period_label_short: "today".to_string(),
                timeline_title: "Today timeline".to_string(),
                start,
                end: now,
                previous_start,
                previous_end: start,
                bucket_count: 24,
            }
        }
        "yesterday" => {
            let today_start = start_of_day(now);
            let start = today_start - Duration::days(1);
            let end = today_start;
            TimeRange {
                key: "yesterday".to_string(),
                label: "Yesterday".to_string(),
                period_label: "yesterday".to_string(),
                period_label_short: "yesterday".to_string(),
                timeline_title: "Yesterday timeline".to_string(),
                start,
                end,
                previous_start: start - Duration::days(1),
                previous_end: start,
                bucket_count: 24,
            }
        }
        "week" => {
            let start = start_of_week(now);
            TimeRange {
                key: "week".to_string(),
                label: "This week".to_string(),
                period_label: "this week".to_string(),
                period_label_short: "this week".to_string(),
                timeline_title: "Week timeline".to_string(),
                start,
                end: now,
                previous_start: start - Duration::days(7),
                previous_end: start,
                bucket_count: 14,
            }
        }
        "7days" => {
            let start = now - Duration::days(7);
            TimeRange {
                key: "7days".to_string(),
                label: "Last 7 days".to_string(),
                period_label: "the last 7 days".to_string(),
                period_label_short: "7 days".to_string(),
                timeline_title: "Last 7 days timeline".to_string(),
                start,
                end: now,
                previous_start: start - Duration::days(7),
                previous_end: start,
                bucket_count: 14,
            }
        }
        "30days" => {
            let start = now - Duration::days(30);
            TimeRange {
                key: "30days".to_string(),
                label: "Last 30 days".to_string(),
                period_label: "the last 30 days".to_string(),
                period_label_short: "30 days".to_string(),
                timeline_title: "Last 30 days timeline".to_string(),
                start,
                end: now,
                previous_start: start - Duration::days(30),
                previous_end: start,
                bucket_count: 15,
            }
        }
        _ => {
            let start = now - Duration::minutes(10);
            TimeRange {
                key: "10m".to_string(),
                label: "Last 10 minutes".to_string(),
                period_label: "the last 10 minutes".to_string(),
                period_label_short: "10m".to_string(),
                timeline_title: "Last 10 minutes timeline".to_string(),
                start,
                end: now,
                previous_start: start - Duration::minutes(10),
                previous_end: start,
                bucket_count: 10,
            }
        }
    }
}

fn start_of_day(now: DateTime<Utc>) -> DateTime<Utc> {
    let naive = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid start of day");
    Utc.from_utc_datetime(&naive)
}

fn start_of_week(now: DateTime<Utc>) -> DateTime<Utc> {
    let weekday = now.weekday();
    let days_from_monday = weekday.num_days_from_monday() as i64;
    let seconds = now.time().num_seconds_from_midnight() as i64;
    let midnight = now - Duration::seconds(seconds);
    start_of_day(midnight - Duration::days(days_from_monday))
}
