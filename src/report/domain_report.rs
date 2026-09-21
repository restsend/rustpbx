//! Domain-scoped report queries behind `/console/api/reports/v2/{domain}`.
//!
//! Domains: call (core CDR), trunk (per-trunk traffic), route (per-route
//! traffic), voicemail (addon tables, fault-tolerant when the addon is
//! disabled) and locator (live registry + runtime-snapshot trends).
//!
//! All time bucketing goes through [`crate::report`] so SQLite / MySQL /
//! PostgreSQL stay consistent. Zero schema assumptions beyond tables that
//! already exist.

use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, QueryFilter,
    QueryOrder, QuerySelect,
};
use sea_orm::sea_query::Expr;

use crate::models::call_record::{Column as CdrCol, Entity as CdrEntity};
use crate::report::{bucket_index_expr, bucket_start_utc, pct, sum_i64, TimeBucket};
use crate::utils::count_when;

const ANSWERED_STATUSES: [&str; 2] = ["answered", "completed"];

/// Shared query parameters for domain reports.
#[derive(Debug, Clone)]
pub struct DomainQuery {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket: TimeBucket,
    pub tz_offset_secs: i64,
    /// Optional direction filter (inbound / outbound / internal).
    pub direction: Option<String>,
}

impl DomainQuery {
    fn window(&self) -> sea_orm::sea_query::Condition {
        let mut c = sea_orm::sea_query::Condition::all()
            .add(CdrCol::StartedAt.gte(self.from))
            .add(CdrCol::StartedAt.lt(self.to));
        if let Some(ref d) = self.direction {
            c = c.add(CdrCol::Direction.eq(d.clone()));
        }
        c
    }

    fn primary_legs_only() -> sea_orm::sea_query::Condition {
        // Primary CDR = logical-call root (session_id NULL or == call_id).
        sea_orm::sea_query::Condition::all().add(
            CdrCol::SessionId
                .is_null()
                .or(Expr::col(CdrCol::SessionId).eq(Expr::col(CdrCol::CallId))),
        )
    }
}

// ════════════════════════════════════════════════════════════════════
// call — core CDR series + breakdowns
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallSeriesPoint {
    pub bucket_start: String,
    pub total: i64,
    pub answered: i64,
    pub missed: i64,
    pub total_duration_secs: i64,
    pub avg_duration_secs: f64,
    pub asr_pct: f64,
}

#[derive(Debug, FromQueryResult)]
struct CallSeriesRow {
    #[sea_orm(column = "bucket")]
    bucket: i64,
    total: i64,
    answered: i64,
    duration_sum: Option<i64>,
}

/// Per-bucket call volume / ASR / duration over primary CDR legs.
pub async fn call_series<C>(conn: &C, q: &DomainQuery) -> anyhow::Result<Vec<CallSeriesPoint>>
where
    C: ConnectionTrait,
{
    let bucket_expr =
        bucket_index_expr(conn.get_database_backend(), "started_at", q.bucket.secs(), q.tz_offset_secs);
    let answered = CdrCol::Status.is_in(ANSWERED_STATUSES);
    let rows: Vec<CallSeriesRow> = CdrEntity::find()
        .select_only()
        .column_as(bucket_expr, "bucket")
        .column_as(CdrCol::Id.count(), "total")
        .column_as(count_when(answered.clone()), "answered")
        .column_as(
            sum_i64(
                conn,
                Expr::case(answered, Expr::col(CdrCol::DurationSecs)).finally(0).into(),
            ),
            "duration_sum",
        )
        .filter(q.window())
        .filter(DomainQuery::primary_legs_only())
        .group_by(Expr::cust("bucket"))
        .order_by_asc(Expr::cust("bucket"))
        .into_model()
        .all(conn)
        .await
        .map_err(|e| anyhow::anyhow!("call series: {e}"))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let answered = std::cmp::Ord::max(r.answered, 0);
            let duration = r.duration_sum.unwrap_or(0);
            CallSeriesPoint {
                bucket_start: bucket_start_utc(r.bucket, q.bucket.secs(), q.tz_offset_secs)
                    .to_rfc3339(),
                total: r.total,
                answered,
                missed: std::cmp::Ord::max(r.total - answered, 0),
                total_duration_secs: duration,
                avg_duration_secs: if answered > 0 {
                    duration as f64 / answered as f64
                } else {
                    0.0
                },
                asr_pct: pct(answered as f64, r.total as f64),
            }
        })
        .collect())
}

#[derive(Debug, FromQueryResult)]
struct NameCountRow {
    name: Option<String>,
    count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NameCount {
    /// Label (hangup reason / SIP code / caller number); `None` renders as
    /// the caller-side "(unknown)".
    pub name: Option<String>,
    pub count: i64,
}

async fn grouped_count<C>(
    conn: &C,
    q: &DomainQuery,
    column_sql: &str,
    limit: Option<u64>,
) -> anyhow::Result<Vec<NameCount>>
where
    C: ConnectionTrait,
{
    let mut query = CdrEntity::find()
        .select_only()
        .column_as(Expr::cust(column_sql.to_string()), "name")
        .column_as(CdrCol::Id.count(), "count")
        .filter(q.window())
        .filter(DomainQuery::primary_legs_only())
        .group_by(Expr::cust(column_sql.to_string()))
        .order_by_desc(Expr::cust("count"));
    if let Some(l) = limit {
        query = query.limit(l);
    }
    let rows: Vec<NameCountRow> = query
        .into_model()
        .all(conn)
        .await
        .map_err(|e| anyhow::anyhow!("grouped count {column_sql}: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|r| NameCount { name: r.name, count: r.count })
        .collect())
}

/// Hangup-reason distribution over the window.
pub async fn hangup_reason_breakdown<C>(conn: &C, q: &DomainQuery) -> anyhow::Result<Vec<NameCount>>
where
    C: ConnectionTrait,
{
    grouped_count(conn, q, "hangup_reason", None).await
}

/// Final SIP status code TopN — the first signal of trunk-side problems.
pub async fn sip_status_breakdown<C>(
    conn: &C,
    q: &DomainQuery,
    limit: u64,
) -> anyhow::Result<Vec<NameCount>>
where
    C: ConnectionTrait,
{
    // CAST to TEXT so the integer column decodes into the String label
    // (a raw INTEGER fails Option<String> decoding in sea-orm).
    let rows = grouped_count(conn, q, "CAST(sip_status_code AS TEXT)", Some(limit)).await?;
    Ok(rows
        .into_iter()
        .map(|r| NameCount { name: r.name.map(|c| format!("SIP {c}")), count: r.count })
        .collect())
}

/// Top callers/callees by count.
pub async fn top_numbers<C>(
    conn: &C,
    q: &DomainQuery,
    limit: u64,
) -> anyhow::Result<Vec<NameCount>>
where
    C: ConnectionTrait,
{
    grouped_count(conn, q, "from_number", Some(limit)).await
}

// ════════════════════════════════════════════════════════════════════
// trunk — per-trunk traffic (in via sip_trunk_id, out via outbound)
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrunkReportRow {
    pub trunk_id: Option<i64>,
    pub name: String,
    pub carrier: Option<String>,
    pub offered: i64,
    pub answered: i64,
    pub missed: i64,
    pub total_duration_secs: i64,
    pub asr_pct: f64,
}

#[derive(Debug, FromQueryResult)]
struct TrunkAggRow {
    #[sea_orm(column = "trunk_ref")]
    trunk_ref: Option<i64>,
    total: i64,
    answered: i64,
    duration_sum: Option<i64>,
}

/// Per-trunk traffic report.
///
/// Day buckets read the pre-aggregated [`rustpbx_cdr_daily`] rollup (whose
/// `sip_trunk_id` column already folds the outbound leg); sub-day buckets
/// aggregate the raw CDR with the same CASE fold. Trunk names join from
/// `rustpbx_sip_trunks` in Rust (trunk counts are small).
pub async fn trunk_report<C>(conn: &C, q: &DomainQuery) -> anyhow::Result<Vec<TrunkReportRow>>
where
    C: ConnectionTrait,
{
    let use_daily = q.bucket == TimeBucket::Day;
    let agg_rows: Vec<TrunkAggRow> = if use_daily {
        // cdr_daily: day window + direction fold already applied at rollup time.
        let query = crate::models::cdr_daily::Entity::find()
            .select_only()
            .column_as(crate::models::cdr_daily::Column::SipTrunkId, "trunk_ref")
            .column_as(
                sum_i64(conn, Expr::col(crate::models::cdr_daily::Column::TotalCalls)),
                "total",
            )
            .column_as(
                sum_i64(conn, Expr::col(crate::models::cdr_daily::Column::Answered)),
                "answered",
            )
            .column_as(
                sum_i64(
                    conn,
                    Expr::col(crate::models::cdr_daily::Column::TotalDurationSecs),
                ),
                "duration_sum",
            )
            .filter(crate::models::cdr_daily::Column::Day.gte(q.from.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()))
            .filter(crate::models::cdr_daily::Column::Day.lt(q.to))
            .group_by(crate::models::cdr_daily::Column::SipTrunkId)
            .into_model::<TrunkAggRow>();
        query
            .all(conn)
            .await
            .map_err(|e| anyhow::anyhow!("trunk daily: {e}"))?
    } else {
        let answered = CdrCol::Status.is_in(ANSWERED_STATUSES);
        let trunk_dim: sea_orm::sea_query::SimpleExpr = Expr::case(
            CdrCol::Direction.eq("outbound"),
            Expr::col(CdrCol::OutboundSipTrunkId),
        )
        .finally(Expr::col(CdrCol::SipTrunkId))
        .into();
        CdrEntity::find()
            .select_only()
            .column_as(trunk_dim, "trunk_ref")
            .column_as(CdrCol::Id.count(), "total")
            .column_as(count_when(answered.clone()), "answered")
            .column_as(
                sum_i64(
                    conn,
                    Expr::case(answered, Expr::col(CdrCol::DurationSecs)).finally(0).into(),
                ),
                "duration_sum",
            )
            .filter(q.window())
            .filter(DomainQuery::primary_legs_only())
            .group_by(Expr::cust("trunk_ref"))
            .into_model::<TrunkAggRow>()
            .all(conn)
            .await
            .map_err(|e| anyhow::anyhow!("trunk cdr: {e}"))?
    };

    // Dimension names (small table — fetch all).
    use crate::models::sip_trunk;
    let trunks: std::collections::HashMap<i64, sip_trunk::Model> = sip_trunk::Entity::find()
        .all(conn)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| (t.id, t))
        .collect();

    Ok(agg_rows
        .into_iter()
        .map(|r| {
            let answered = std::cmp::Ord::max(r.answered, 0);
            let duration = r.duration_sum.unwrap_or(0);
            let (name, carrier) = match r.trunk_ref.and_then(|id| trunks.get(&id)) {
                Some(t) => (
                    t.display_name.clone().unwrap_or_else(|| t.name.clone()),
                    t.carrier.clone(),
                ),
                None => (
                    match r.trunk_ref {
                        Some(id) => format!("#{}", id),
                        None => "(无中继)".to_string(),
                    },
                    None,
                ),
            };
            TrunkReportRow {
                trunk_id: r.trunk_ref,
                name,
                carrier,
                offered: r.total,
                answered,
                missed: std::cmp::Ord::max(r.total - answered, 0),
                total_duration_secs: duration,
                asr_pct: pct(answered as f64, r.total as f64),
            }
        })
        .collect())
}

// ════════════════════════════════════════════════════════════════════
// route — per-route traffic (NULL route_id = unmatched)
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteReportRow {
    pub route_id: Option<i64>,
    pub name: String,
    pub offered: i64,
    pub answered: i64,
    pub missed: i64,
    pub total_duration_secs: i64,
    pub asr_pct: f64,
}

#[derive(Debug, FromQueryResult)]
struct RouteAggRow {
    route_id: Option<i64>,
    total: i64,
    answered: i64,
    duration_sum: Option<i64>,
}

pub async fn route_report<C>(conn: &C, q: &DomainQuery) -> anyhow::Result<Vec<RouteReportRow>>
where
    C: ConnectionTrait,
{
    let answered = CdrCol::Status.is_in(ANSWERED_STATUSES);
    let rows: Vec<RouteAggRow> = CdrEntity::find()
        .select_only()
        .column_as(CdrCol::RouteId, "route_id")
        .column_as(CdrCol::Id.count(), "total")
        .column_as(count_when(answered.clone()), "answered")
        .column_as(
            sum_i64(
                conn,
                Expr::case(answered, Expr::col(CdrCol::DurationSecs)).finally(0).into(),
            ),
            "duration_sum",
        )
        .filter(q.window())
        .filter(DomainQuery::primary_legs_only())
        .group_by(CdrCol::RouteId)
        .order_by_desc(Expr::cust("total"))
        .into_model()
        .all(conn)
        .await
        .map_err(|e| anyhow::anyhow!("route report: {e}"))?;

    use crate::models::routing;
    let routes: std::collections::HashMap<i64, routing::Model> = routing::Entity::find()
        .all(conn)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.id, r))
        .collect();

    Ok(rows
        .into_iter()
        .map(|r| {
            let answered = std::cmp::Ord::max(r.answered, 0);
            let duration = r.duration_sum.unwrap_or(0);
            let name = r
                .route_id
                .and_then(|id| routes.get(&id))
                .map(|route| route.name.clone())
                .or_else(|| r.route_id.map(|id| format!("#{id}")))
                .unwrap_or_else(|| "未命中路由".to_string());
            RouteReportRow {
                route_id: r.route_id,
                name,
                offered: r.total,
                answered,
                missed: std::cmp::Ord::max(r.total - answered, 0),
                total_duration_secs: duration,
                asr_pct: pct(answered as f64, r.total as f64),
            }
        })
        .collect())
}

// ════════════════════════════════════════════════════════════════════
// voicemail — addon tables via raw SQL (commerce-gated; tolerant when the
// tables are absent so the endpoint degrades to enabled=false)
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VoicemailSummary {
    pub enabled: bool,
    pub total: i64,
    pub unread: i64,
    pub read_ratio_pct: f64,
    pub avg_duration_secs: f64,
    pub transcript_coverage_pct: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VoicemailTrendPoint {
    pub bucket_start: String,
    pub count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VoicemailBoxRow {
    pub extension: String,
    pub total: i64,
    pub unread: i64,
}

/// Voicemail overview + trend + per-mailbox backlog.
pub async fn voicemail_report<C>(
    conn: &C,
    q: &DomainQuery,
) -> anyhow::Result<(VoicemailSummary, Vec<VoicemailTrendPoint>, Vec<VoicemailBoxRow>)>
where
    C: ConnectionTrait,
{
    let backend = conn.get_database_backend();
    let mut summary = VoicemailSummary::default();

    // Summary (single aggregate query). Any error (table missing = addon
    // disabled) degrades to the disabled default.
    let sql = "SELECT COUNT(*) AS c, \
               SUM(CASE WHEN \"read\" = FALSE OR \"read\" = 0 THEN 1 ELSE 0 END) AS unread, \
               AVG(duration) AS avg_dur, \
               SUM(CASE WHEN transcript IS NOT NULL AND transcript <> '' THEN 1 ELSE 0 END) AS with_ts \
               FROM rustpbx_voicemail_message WHERE created_at >= ? AND created_at < ?";
    let row = match conn
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            backend,
            sql,
            [q.from.into(), q.to.into()],
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "voicemail tables unavailable; reporting disabled");
            return Ok((summary, Vec::new(), Vec::new()));
        }
    };
    let Some(row) = row else {
        return Ok((summary, Vec::new(), Vec::new()));
    };
    let total: i64 = row.try_get("", "c").unwrap_or(0);
    summary.enabled = true;
    summary.total = total;
    summary.unread = row.try_get::<i64>("", "unread").unwrap_or(0);
    summary.read_ratio_pct = pct(std::cmp::Ord::max(total - summary.unread, 0) as f64, total as f64);
    summary.avg_duration_secs = row.try_get::<f64>("", "avg_dur").unwrap_or(0.0);
    let with_ts: i64 = row.try_get("", "with_ts").unwrap_or(0);
    summary.transcript_coverage_pct = pct(with_ts as f64, total as f64);

    // Trend (bucketed counts).
    let bucket_expr = crate::report::bucket_index_sql(
        backend,
        "created_at",
        q.bucket.secs(),
        q.tz_offset_secs,
    );
    let trend_sql = format!(
        "SELECT {bucket_expr} AS bucket, COUNT(*) AS c FROM rustpbx_voicemail_message \
         WHERE created_at >= ? AND created_at < ? GROUP BY bucket ORDER BY bucket ASC"
    );
    let trend_rows = conn
        .query_all_raw(sea_orm::Statement::from_sql_and_values(
            backend,
            trend_sql,
            [q.from.into(), q.to.into()],
        ))
        .await
        .unwrap_or_default();
    let mut trend = Vec::new();
    for r in &trend_rows {
        let bucket: i64 = r.try_get("", "bucket").unwrap_or(0);
        let count: i64 = r.try_get("", "c").unwrap_or(0);
        trend.push(VoicemailTrendPoint {
            bucket_start: bucket_start_utc(bucket, q.bucket.secs(), q.tz_offset_secs).to_rfc3339(),
            count,
        });
    }

    // Per-mailbox backlog Top10.
    let box_sql = "SELECT b.extension AS extension, COUNT(m.id) AS c, \
                   SUM(CASE WHEN m.\"read\" = FALSE OR m.\"read\" = 0 THEN 1 ELSE 0 END) AS unread \
                   FROM rustpbx_voicemail_message m \
                   JOIN rustpbx_voicemail_box b ON b.id = m.box_id \
                   WHERE m.created_at >= ? AND m.created_at < ? \
                   GROUP BY b.extension ORDER BY unread DESC, c DESC LIMIT 10";
    let box_rows = conn
        .query_all_raw(sea_orm::Statement::from_sql_and_values(
            backend,
            box_sql,
            [q.from.into(), q.to.into()],
        ))
        .await
        .unwrap_or_default();
    let mut boxes = Vec::new();
    for r in &box_rows {
        boxes.push(VoicemailBoxRow {
            extension: r.try_get("", "extension").unwrap_or_default(),
            total: r.try_get("", "c").unwrap_or(0),
            unread: r.try_get("", "unread").unwrap_or(0),
        });
    }

    Ok((summary, trend, boxes))
}

// ════════════════════════════════════════════════════════════════════
// locator — live stats + snapshot trend
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LocatorLive {
    pub available: bool,
    pub online_locations: i64,
    pub online_users: i64,
    pub webrtc_locations: i64,
    pub by_transport: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LocatorTrendPoint {
    pub bucket_start: String,
    pub online_locations: f64,
    pub online_users: f64,
    pub webrtc_locations: f64,
}

/// Bucketed averages over the runtime snapshots (`kind = 'locator'`).
pub async fn locator_trend<C>(conn: &C, q: &DomainQuery) -> anyhow::Result<Vec<LocatorTrendPoint>>
where
    C: ConnectionTrait,
{
    use crate::models::runtime_snapshot;
    let bucket_expr = bucket_index_expr(
        conn.get_database_backend(),
        "created_at",
        q.bucket.secs(),
        q.tz_offset_secs,
    );

    #[derive(Debug, FromQueryResult)]
    struct TrendRow {
        #[sea_orm(column = "bucket")]
        bucket: i64,
        avg_locations: Option<f64>,
        avg_users: Option<f64>,
        avg_webrtc: Option<f64>,
    }

    let rows: Vec<TrendRow> = runtime_snapshot::Entity::find()
        .select_only()
        .column_as(bucket_expr, "bucket")
        .column_as(
            crate::report::avg_f64(
                conn.get_database_backend(),
                Expr::col(runtime_snapshot::Column::Num1),
            ),
            "avg_locations",
        )
        .column_as(
            crate::report::avg_f64(
                conn.get_database_backend(),
                Expr::col(runtime_snapshot::Column::Num2),
            ),
            "avg_users",
        )
        .column_as(
            crate::report::avg_f64(
                conn.get_database_backend(),
                Expr::col(runtime_snapshot::Column::Num3),
            ),
            "avg_webrtc",
        )
        .filter(runtime_snapshot::Column::Kind.eq("locator"))
        .filter(
            runtime_snapshot::Column::CreatedAt
                .gte(q.from)
                .and(runtime_snapshot::Column::CreatedAt.lt(q.to)),
        )
        .group_by(Expr::cust("bucket"))
        .order_by_asc(Expr::cust("bucket"))
        .into_model()
        .all(conn)
        .await
        .map_err(|e| anyhow::anyhow!("locator trend: {e}"))?;

    Ok(rows
        .into_iter()
        .map(|r| LocatorTrendPoint {
            bucket_start: bucket_start_utc(r.bucket, q.bucket.secs(), q.tz_offset_secs).to_rfc3339(),
            online_locations: r.avg_locations.unwrap_or(0.0),
            online_users: r.avg_users.unwrap_or(0.0),
            webrtc_locations: r.avg_webrtc.unwrap_or(0.0),
        })
        .collect())
}
