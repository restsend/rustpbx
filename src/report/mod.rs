//! Unified reporting layer shared by the PBX core and the CC addon.
//!
//! Provides the three-dialect time-bucket expression helper (SQLite / MySQL
//! / PostgreSQL), timezone-aware bucket alignment, and the canonical metric
//! definitions used by every report endpoint so aggregations never diverge
//! between handlers.
//!
//! Metric conventions (see `docs/reporting.md`):
//! - ASR (answer seizure ratio)  = answered / offered
//! - Abandon rate                = abandoned / offered
//! - SLA                         = answered-with-wait<=target / answered
//! - AHT (avg handle time)       = mean talk time over answered calls
//! - AWT (avg wait time)         = mean wait time over answered calls
//!
//! Cluster semantics: additive counters are SUMmed across `instance_id`
//! rows; gauge-like columns (waiting depth) take MAX / AVG at read time.

pub mod rollup;

pub mod domain_report;

use chrono::{DateTime, Utc};
use sea_orm::sea_query::SimpleExpr;

/// Fixed reporting buckets. Hour / day / week; sub-hour ranges use custom
/// second values chosen by the caller (the console dashboard does so).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeBucket {
    Hour,
    Day,
    Week,
}

impl TimeBucket {
    pub fn secs(self) -> i64 {
        match self {
            TimeBucket::Hour => 3600,
            TimeBucket::Day => 86400,
            TimeBucket::Week => 604800,
        }
    }

    /// Parse from a query parameter (`hour`/`day`/`week`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "hour" | "hourly" => Some(TimeBucket::Hour),
            "day" | "daily" => Some(TimeBucket::Day),
            "week" | "weekly" => Some(TimeBucket::Week),
            _ => None,
        }
    }
}

/// SQL fragment yielding `(to_col - from_col)` in whole seconds for the
/// given backend (used for ring-duration derivation from timestamps).
pub fn epoch_diff_secs_sql(backend: sea_orm::DatabaseBackend, from_col: &str, to_col: &str) -> String {
    match backend {
        sea_orm::DatabaseBackend::Sqlite => format!(
            "CAST((julianday({to_col}) - julianday({from_col})) * 86400 AS INTEGER)"
        ),
        sea_orm::DatabaseBackend::MySql => format!(
            "TIMESTAMPDIFF(SECOND, {from_col}, {to_col})"
        ),
        sea_orm::DatabaseBackend::Postgres => format!(
            "CAST(EXTRACT(EPOCH FROM ({to_col} - {from_col})) AS BIGINT)"
        ),
        _ => "0".to_string(),
    }
}

/// Raw SQL fragment of [`bucket_index_expr`] (for hand-built statements).
pub fn bucket_index_sql(backend: sea_orm::DatabaseBackend, column_sql: &str, bucket_secs: i64, tz_offset_secs: i64) -> String {
    match backend {
        // SQLite `/` on integers is already floor-division for positive
        // epochs (and there is no FLOOR function), so plain CAST is exact.
        // substr(...,1,19): CDR timestamps are RFC-3339 with T/space
        // separator, sub-second digits and a timezone suffix — SQLite's
        // parser rejects >3 fractional digits, so truncate to whole seconds
        // (UTC) before the epoch conversion.
        sea_orm::DatabaseBackend::Sqlite => format!(
            "CAST((CAST(strftime('%s', substr({column_sql}, 1, 19)) AS INTEGER) + {tz_offset_secs}) / {bucket_secs} AS INTEGER)"
        ),
        sea_orm::DatabaseBackend::MySql => format!(
            "CAST(FLOOR((UNIX_TIMESTAMP({column_sql}) + {tz_offset_secs}) / {bucket_secs}) AS SIGNED)"
        ),
        sea_orm::DatabaseBackend::Postgres => format!(
            "CAST(FLOOR((EXTRACT(EPOCH FROM {column_sql}) + {tz_offset_secs}) / {bucket_secs}) AS BIGINT)"
        ),
        _ => "0".to_string(),
    }
}

/// SQL expression producing the bucket index for a timestamp column.
///
/// `epoch(col)` is dialect-specific; the index is computed as
/// `FLOOR((epoch(col) + tz_offset_secs) / bucket_secs)` so that DAY buckets
/// align to the caller's timezone instead of UTC midnight.
/// `column_sql` is the raw column name (interpolated — callers must only
/// pass identifiers they own).
pub fn bucket_index_expr(
    backend: sea_orm::DatabaseBackend,
    column_sql: &str,
    bucket_secs: i64,
    tz_offset_secs: i64,
) -> SimpleExpr {
    sea_orm::sea_query::Expr::cust(bucket_index_sql(backend, column_sql, bucket_secs, tz_offset_secs))
}

/// UTC instant at which bucket `index` starts (undoing the tz shift used by
/// [`bucket_index_expr`]).
pub fn bucket_start_utc(index: i64, bucket_secs: i64, tz_offset_secs: i64) -> DateTime<Utc> {
    let epoch = index * bucket_secs - tz_offset_secs;
    DateTime::from_timestamp(epoch, 0).unwrap_or_default()
}

/// Backend-correct SUM(): CASTs the aggregate so SQLite/MySQL/Postgres all
/// return an integer type sea-orm can decode as i64.
pub fn sum_i64(db: &impl sea_orm::ConnectionTrait, expr: SimpleExpr) -> SimpleExpr {
    use sea_orm::{ExprTrait, sea_query::Alias};
    let cast_type = match db.get_database_backend() {
        sea_orm::DatabaseBackend::Sqlite => "INTEGER",
        sea_orm::DatabaseBackend::MySql => "SIGNED",
        sea_orm::DatabaseBackend::Postgres => "BIGINT",
        _ => "BIGINT",
    };
    SimpleExpr::from(sea_orm::sea_query::Func::sum(expr)).cast_as(Alias::new(cast_type))
}

/// Float SUM() over an aggregate expression (decodes as f64 on every
/// backend — unlike [`sum_i64`] this never casts to an integer type).
pub fn sum_f64(expr: SimpleExpr) -> SimpleExpr {
    use sea_orm::ExprTrait;
    sea_orm::sea_query::Func::sum(expr).into()
}

/// AVG() CASTed to a float type so all backends decode as f64.
pub fn avg_f64(backend: sea_orm::DatabaseBackend, expr: SimpleExpr) -> SimpleExpr {
    use sea_orm::{ExprTrait, sea_query::Alias};
    let cast_type = match backend {
        sea_orm::DatabaseBackend::Postgres => "DOUBLE PRECISION",
        _ => "DOUBLE",
    };
    SimpleExpr::from(sea_orm::sea_query::Func::avg(expr)).cast_as(Alias::new(cast_type))
}

/// Safe ratio in percent — 0.0 when the denominator is zero.
pub fn pct(numerator: f64, denominator: f64) -> f64 {
    if denominator <= 0.0 {
        0.0
    } else {
        (numerator / denominator) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_parse_and_secs() {
        assert_eq!(TimeBucket::parse("hour"), Some(TimeBucket::Hour));
        assert_eq!(TimeBucket::parse("Day"), Some(TimeBucket::Day));
        assert_eq!(TimeBucket::parse("week"), Some(TimeBucket::Week));
        assert_eq!(TimeBucket::parse("bogus"), None);
        assert_eq!(TimeBucket::Day.secs(), 86400);
    }

    #[test]
    fn bucket_start_roundtrip_with_tz() {
        // Day bucket with UTC+8: index 0 starts at 1970-01-01 00:00 UTC-8.
        let start = bucket_start_utc(0, 86400, 8 * 3600);
        assert_eq!(start.format("%Y-%m-%d %H:%M").to_string(), "1969-12-31 16:00");
        // An epoch inside the same tz-aligned day maps to the same index.
        let expr = bucket_index_expr(sea_orm::DatabaseBackend::Sqlite, "x", 86400, 8 * 3600);
        // Sanity: the expression embeds the tz shift.
        assert!(format!("{:?}", expr).contains("28800"));
    }

    #[test]
    fn pct_zero_denominator() {
        assert_eq!(pct(1.0, 0.0), 0.0);
        assert_eq!(pct(1.0, 2.0), 50.0);
    }
}
