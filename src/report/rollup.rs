//! Daily CDR rollup (`rustpbx_cdr_daily`): idempotent recomputation from
//! `rustpbx_call_records` for a given day, dimensioned by direction,
//! department, and trunk.

use chrono::{DateTime, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, QueryFilter,
    QuerySelect, Set,
};
use sea_orm::sea_query::Expr;

use crate::models::call_record::{Column as CdrCol, Entity as CdrEntity};
use crate::models::cdr_daily;
use crate::utils::count_when;

#[derive(Debug, FromQueryResult)]
struct DailyAggRow {
    direction: String,
    department_id: Option<i64>,
    effective_trunk_id: Option<i64>,
    total_calls: i64,
    answered: i64,
    total_duration: Option<i64>,
}

/// Recompute `rustpbx_cdr_daily` rows for `[day_start, day_start + 1d)`.
/// Existing rows for the window are deleted and re-inserted (simplest
/// idempotent semantics; the row count per day is bounded by the
/// direction × department × trunk combinations actually seen).
///
/// Returns the number of rollup rows written.
pub async fn rollup_cdr_daily<C>(conn: &C, day_start: DateTime<Utc>) -> anyhow::Result<usize>
where
    C: ConnectionTrait,
{
    let day_end = day_start + chrono::Duration::days(1);
    let answered_status: Vec<&str> = vec!["answered", "completed"];

    // Aggregate from the primary CDR rows only: root sessions are where
    // per-logical-call counting is meaningful (child legs share session_id).
    // The trunk dimension folds the outbound leg into the same column:
    // outbound calls carry their trunk in `outbound_sip_trunk_id`, so
    // `sip_trunk_id` on a rollup row means "the trunk this call rode" —
    // the `direction` column distinguishes in/out without a schema change.
    let trunk_dim: sea_orm::sea_query::SimpleExpr = sea_orm::sea_query::Expr::case(
        CdrCol::Direction.eq("outbound"),
        sea_orm::sea_query::Expr::col(CdrCol::OutboundSipTrunkId),
    )
    .finally(sea_orm::sea_query::Expr::col(CdrCol::SipTrunkId))
    .into();
    let rows: Vec<DailyAggRow> = CdrEntity::find()
        .select_only()
        .column_as(CdrCol::Direction, "direction")
        .column_as(CdrCol::DepartmentId, "department_id")
        .column_as(trunk_dim, "effective_trunk_id")
        .column_as(CdrCol::Id.count(), "total_calls")
        .column_as(count_when(CdrCol::Status.is_in(answered_status.clone())), "answered")
        .column_as(
            crate::report::sum_i64(
                conn,
                sea_orm::sea_query::Expr::case(
                    CdrCol::Status.is_in(answered_status.clone()),
                    sea_orm::sea_query::Expr::col(CdrCol::DurationSecs),
                )
                .finally(0)
                .into(),
            ),
            "total_duration",
        )
        .filter(CdrCol::StartedAt.gte(day_start))
        .filter(CdrCol::StartedAt.lt(day_end))
        .filter(
            // Primary CDR = logical-call root (see call_record.session_id doc).
            CdrCol::SessionId
                .is_null()
                .or(Expr::col(CdrCol::SessionId).eq(Expr::col(CdrCol::CallId))),
        )
        .group_by(CdrCol::Direction)
        .group_by(CdrCol::DepartmentId)
        // Group by the selected expression's unambiguous alias. Repeating the
        // CASE binds "outbound" twice; MySQL strict grouping treats those
        // placeholders as different expressions.
        .group_by(Expr::col(sea_orm::sea_query::Alias::new("effective_trunk_id")))
        .into_model()
        .all(conn)
        .await
        .map_err(|e| anyhow::anyhow!("cdr daily aggregate: {e}"))?;

    use sea_orm::ActiveModelTrait;
    // Idempotent refresh: drop the window, then write fresh aggregates.
    cdr_daily::Entity::delete_many()
        .filter(cdr_daily::Column::Day.gte(day_start))
        .filter(cdr_daily::Column::Day.lt(day_end))
        .exec(conn)
        .await
        .map_err(|e| anyhow::anyhow!("cdr daily reset: {e}"))?;

    let now = Utc::now();
    for r in &rows {
        let answered = std::cmp::Ord::max(r.answered, 0);
        let total_duration = r.total_duration.unwrap_or(0);
        let model = cdr_daily::ActiveModel {
            id: sea_orm::NotSet,
            day: Set(day_start),
            direction: Set(r.direction.clone()),
            department_id: Set(r.department_id),
            sip_trunk_id: Set(r.effective_trunk_id),
            total_calls: Set(r.total_calls),
            answered: Set(answered),
            missed: Set(std::cmp::Ord::max(r.total_calls - answered, 0)),
            total_duration_secs: Set(total_duration),
            avg_duration_secs: Set(if answered > 0 {
                total_duration as f64 / answered as f64
            } else {
                0.0
            }),
            updated_at: Set(now),
        };
        model
            .insert(conn)
            .await
            .map_err(|e| anyhow::anyhow!("cdr daily insert: {e}"))?;
    }
    Ok(rows.len())
}

/// Rollup "today" and "yesterday" — the two windows whose aggregates can
/// still change. Cheap enough to run every few minutes.
pub async fn rollup_recent_days<C>(conn: &C) -> anyhow::Result<usize>
where
    C: ConnectionTrait,
{
    let now = Utc::now();
    let today = day_start(now);
    let yesterday = day_start(now - chrono::Duration::days(1));
    let a = rollup_cdr_daily(conn, yesterday).await?;
    let b = rollup_cdr_daily(conn, today).await?;
    Ok(a + b)
}

fn day_start(t: DateTime<Utc>) -> DateTime<Utc> {
    t.date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap_or_default()
        .and_utc()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::call_record;
    use chrono::TimeZone;
    use sea_orm::{ActiveModelTrait, Database, Set};
    use sea_orm_migration::MigratorTrait;

    fn cdr_row(call_id: &str, direction: &str, status: &str, duration: i32, started: DateTime<Utc>) -> call_record::ActiveModel {
        call_record::ActiveModel {
            id: sea_orm::NotSet,
            call_id: Set(call_id.to_string()),
            session_id: Set(Some(call_id.to_string())), // primary/root leg
            display_id: Set(None),
            direction: Set(direction.to_string()),
            status: Set(status.to_string()),
            started_at: Set(started),
            ended_at: Set(Some(started + chrono::Duration::seconds(duration as i64))),
            duration_secs: Set(duration),
            from_number: Set(Some("1001".into())),
            to_number: Set(Some("1002".into())),
            caller_name: Set(None),
            agent_name: Set(None),
            queue: Set(None),
            department_id: Set(None),
            extension_id: Set(None),
            sip_trunk_id: Set(None),
            outbound_sip_trunk_id: Set(None),
            route_id: Set(None),
            sip_gateway: Set(None),
            rewrite_original_from: Set(None),
            rewrite_original_to: Set(None),
            caller_uri: Set(None),
            callee_uri: Set(None),
            recording_url: Set(None),
            recording_duration_secs: Set(None),
            has_transcript: Set(false),
            transcript_status: Set("none".into()),
            transcript_language: Set(None),
            tags: Set(None),
            leg_timeline: Set(None),
            metadata: Set(None),
            hangup_reason: Set(Some("caller".into())),
            sip_status_code: Set(Some(200)),
            created_at: Set(started),
            updated_at: Set(started),
            archived_at: Set(None),
        }
    }

    #[tokio::test]
    async fn rollup_aggregates_primary_cdrs_by_dims() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::models::migration::Migrator::up(&db, None).await.unwrap();

        let day = Utc.with_ymd_and_hms(2026, 9, 18, 10, 0, 0).unwrap();
        for (i, (dir, status, dur)) in [
            ("inbound", "answered", 60),
            ("inbound", "answered", 120),
            ("inbound", "failed", 0),
            ("outbound", "answered", 30),
        ]
        .into_iter()
        .enumerate()
        {
            cdr_row(&format!("r-{i}"), dir, status, dur, day)
                .insert(&db)
                .await
                .unwrap();
        }
        // A child leg must be excluded (session_id != call_id).
        let mut child = cdr_row("r-child", "inbound", "answered", 999, day);
        child.session_id = Set(Some("r-0".to_string()));
        child.insert(&db).await.unwrap();

        rollup_cdr_daily(&db, day_start(day)).await.unwrap();

        use sea_orm::EntityTrait;
        let rows = cdr_daily::Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 2, "one row per direction: {:?}", rows);
        let inbound = rows.iter().find(|r| r.direction == "inbound").unwrap();
        assert_eq!(inbound.total_calls, 3, "child legs excluded");
        assert_eq!(inbound.answered, 2);
        assert_eq!(inbound.missed, 1);
        assert_eq!(inbound.total_duration_secs, 180);
        assert!((inbound.avg_duration_secs - 90.0).abs() < 0.001);
        assert_eq!(inbound.department_id, None);
        assert_eq!(inbound.sip_trunk_id, None);
        let outbound = rows.iter().find(|r| r.direction == "outbound").unwrap();
        assert_eq!(outbound.total_calls, 1);
        assert_eq!(outbound.answered, 1);
    }

    #[tokio::test]
    async fn rollup_groups_effective_trunks_under_strict_sql() {
        use sea_orm::{TransactionTrait, DbBackend, IntoActiveModel};
        // Optional dedicated test database allows exercising MySQL's strict
        // GROUP BY validation, which SQLite does not enforce.
        let url = std::env::var("RUSTPBX_ROLLUP_TEST_URL")
            .unwrap_or_else(|_| "sqlite::memory:".into());
        let db = Database::connect(url).await.unwrap();
        crate::models::migration::Migrator::up(&db, None).await.unwrap();
        let tx = db.begin().await.unwrap();
        if tx.get_database_backend() == DbBackend::MySql {
            tx.execute_unprepared(
                "SET SESSION sql_mode = 'STRICT_TRANS_TABLES,ONLY_FULL_GROUP_BY'")
                .await.unwrap();
        }
        let day = Utc.with_ymd_and_hms(2026, 9, 18, 10, 0, 0).unwrap();
        for id in [1, 2, 7, 8] {
            let mut trunk = crate::models::sip_trunk::Model {
                id, name: format!("rollup-trunk-{id}"),
                created_at: day.into(), updated_at: day.into(), ..Default::default()
            }.into_active_model();
            trunk.id = Set(id);
            trunk.insert(&tx).await.unwrap();
        }
        for (id, direction, inbound, outbound, duration) in [
            ("trunk-a", "outbound", Some(1), Some(7), 30),
            ("trunk-b", "outbound", Some(2), Some(7), 60),
            ("trunk-c", "outbound", Some(1), Some(8), 90),
            ("trunk-d", "inbound", Some(1), Some(7), 120),
            ("trunk-e", "outbound", Some(1), None, 15),
        ] {
            let mut row = cdr_row(id, direction, "answered", duration, day);
            row.sip_trunk_id = Set(inbound);
            row.outbound_sip_trunk_id = Set(outbound);
            row.insert(&tx).await.unwrap();
        }
        for _ in 0..2 {
            assert_eq!(rollup_cdr_daily(&tx, day_start(day)).await.unwrap(), 4);
            let rows = cdr_daily::Entity::find().all(&tx).await.unwrap();
            let merged = rows.iter().find(|r| r.direction == "outbound" && r.sip_trunk_id == Some(7)).unwrap();
            assert_eq!(merged.total_calls, 2);
            assert_eq!(merged.total_duration_secs, 90);
            assert!(rows.iter().any(|r| r.direction == "inbound" && r.sip_trunk_id == Some(1) && r.total_duration_secs == 120));
            assert!(rows.iter().any(|r| r.direction == "outbound" && r.sip_trunk_id.is_none() && r.total_duration_secs == 15));
        }
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn rollup_is_idempotent() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::models::migration::Migrator::up(&db, None).await.unwrap();
        let day = Utc.with_ymd_and_hms(2026, 9, 18, 10, 0, 0).unwrap();
        cdr_row("x-1", "inbound", "answered", 60, day).insert(&db).await.unwrap();

        rollup_cdr_daily(&db, day_start(day)).await.unwrap();
        rollup_cdr_daily(&db, day_start(day)).await.unwrap();

        use sea_orm::EntityTrait;
        let rows = cdr_daily::Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 1, "re-rollup must not duplicate rows");
        assert_eq!(rows[0].total_calls, 1);
    }
}
