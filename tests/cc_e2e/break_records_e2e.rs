//! Break (小休) records pipeline e2e — REAL transitions, strong invariants.
//!
//! Regression gate for prd 2026-10-14 (5-hour phantom "away" records nesting
//! the real episodes, and rings punching through a break): every case drives
//! the PUBLIC transition APIs against a real DB — the history rows are
//! written by the production append path, never hand-seeded — and then walks
//! the records exactly the way `GET /agents/{id}/breaks/records` does
//! (`break_episodes_from_rows` + presence-derived `left_away_at` + default
//! view filters). The invariants under guard:
//!
//! 1. episodes are pairwise DISJOINT — no episode may contain another;
//! 2. every closed episode is BOUNDED (no unbounded accrual, no
//!    hour-scale phantom segments);
//! 3. the automatic `998` no-answer cooldown is hidden by the endpoint's
//!    default view (`include_no_answer=true` / `break_id=998` reveal it);
//! 4. a call interrupting a break SPLITS the episode — the ring/busy gap
//!    must never be swallowed into one giant break record.
//!
//! The 2h budget cap (`BREAK_MAX_SECS_CAP` → deadline sweep force-idle)
//! cannot be waited out here; its arm/fire logic is covered by the sweep
//! unit tests (`cc_phone_tests::break_deadline_self_heal_*`).

use rustpbx::addons::cc::agent::{AgentRegistry, AgentStatus, ResumeTarget};
use rustpbx::addons::cc::models::cc_agent_status_history;
use rustpbx::addons::cc::stats_writer::{break_episodes_from_rows, BreakEpisode};
use sea_orm::{ColumnTrait, Database, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::time::Instant;

async fn setup_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    db
}

/// History rows ordered exactly like the records endpoint walks them
/// (`created_at asc, id asc` — id breaks same-second ties in transition
/// order).
async fn history_rows(db: &DatabaseConnection, agent: &str) -> Vec<cc_agent_status_history::Model> {
    cc_agent_status_history::Entity::find()
        .filter(cc_agent_status_history::Column::AgentId.eq(agent))
        .order_by_asc(cc_agent_status_history::Column::CreatedAt)
        .order_by_asc(cc_agent_status_history::Column::Id)
        .all(db)
        .await
        .expect("query status history")
}

/// Invariant 1 + 2: no episode may contain another, and no closed episode
/// may exceed `max_secs` (real transitions take milliseconds — anything at
/// hour scale is a phantom).
fn assert_disjoint_and_bounded(eps: &[BreakEpisode], max_secs: u64) {
    for (i, a) in eps.iter().enumerate() {
        for b in eps.iter().skip(i + 1) {
            let a_end = a.ended_at.unwrap_or(i64::MAX);
            let b_end = b.ended_at.unwrap_or(i64::MAX);
            assert!(
                a_end <= b.started_at || b_end <= a.started_at,
                "episodes must not overlap/nest: {a:?} vs {b:?}"
            );
        }
    }
    for e in eps {
        if let Some(_ended) = e.ended_at {
            assert!(
                e.secs <= max_secs,
                "closed episode must be bounded (≤{max_secs}s), got {e:?}"
            );
        }
    }
}

/// CASE 1: a realistic day — morning break, an unanswered ring suspends it
/// (release back to Idle), reason switch, a final break — then everything
/// closed. Every record must be a bounded, disjoint, correctly-attributed
/// segment; nothing may merge into a mega-episode.
#[tokio::test]
async fn e2e_full_day_break_flow_records_stay_disjoint_and_bounded() {
    let db = setup_db().await;
    let r = Arc::new(AgentRegistry::with_db(db.clone()));
    r.register("a1".into(), vec![], 1).await.unwrap();
    r.update_status("a1", AgentStatus::Idle).await.unwrap();

    r.update_status("a1", AgentStatus::Away("lunch".into()))
        .await
        .unwrap();
    // Unanswered ring: away → ringing, then the ring terminal releases the
    // reservation (no resume capture → Idle, break NOT restored).
    assert!(
        r.try_reserve_agent("a1", "call-1".into()).await,
        "ring must reach a break-less… idle agent"
    );
    assert!(
        r.ring_no_answer_to_idle("a1", "call-1").await.unwrap(),
        "ring terminal must release the agent"
    );
    r.update_status("a1", AgentStatus::Away("training".into()))
        .await
        .unwrap();
    r.update_status("a1", AgentStatus::Idle).await.unwrap();
    // End the day STILL on break — the open episode is what exercises the
    // presence-authority live tail below.
    r.update_status("a1", AgentStatus::Away("meeting".into()))
        .await
        .unwrap();

    let rows = history_rows(&db, "a1").await;
    assert!(
        rows.len() >= 6,
        "every transition writes exactly one history row, got {}: {rows:?}",
        rows.len()
    );

    let eps = break_episodes_from_rows(&rows, None, chrono::Utc::now());
    let ids: Vec<&str> = eps.iter().map(|e| e.break_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["lunch", "training", "meeting"],
        "chronological, correctly attributed episodes: {eps:?}"
    );
    let (closed, open): (Vec<_>, Vec<_>) =
        eps.iter().partition(|e| e.ended_at.is_some());
    assert_eq!(
        closed.iter().map(|e| e.break_id.as_str()).collect::<Vec<_>>(),
        vec!["lunch", "training"],
        "earlier episodes closed by their leave rows"
    );
    assert_eq!(open.len(), 1, "the current break is the live tail");
    assert_disjoint_and_bounded(&eps, 300);

    // Live-tail semantics over the SAME real rows: the endpoint derives
    // `left_away_at` from the presence row (agent left away at T) — the tail
    // must close exactly there (presence is the authority). Cross the second
    // boundary first: the walker's live-tail guard requires end > started,
    // and every transition above landed within the same second.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let now = chrono::Utc::now();
    let eps_closed = break_episodes_from_rows(&rows, Some(now), now);
    let last_closed = eps_closed.last().expect("closed by presence");
    assert!(
        last_closed.ended_at == Some(now.timestamp())
            && last_closed.started_at <= now.timestamp(),
        "presence must close the live tail: {last_closed:?}"
    );
    assert_disjoint_and_bounded(&eps_closed, 300);
}

/// CASE 2 (the reported complaint shape): a call ANSWERED mid-break must
/// SPLIT the episode — the ring/busy gap must not be swallowed into one
/// giant break record spanning the call.
#[tokio::test]
async fn e2e_answered_call_splits_break_episode_no_mega_record() {
    let db = setup_db().await;
    let r = Arc::new(AgentRegistry::with_db(db.clone()));
    r.register("a1".into(), vec![], 1).await.unwrap();
    r.update_status("a1", AgentStatus::Idle).await.unwrap();

    r.update_status("a1", AgentStatus::Away("break".into()))
        .await
        .unwrap();
    assert!(r.try_reserve_agent("a1", "call-2".into()).await);
    // The agent answers: ringing → busy (real call). The only legal exit
    // from a call is through wrapup (Busy → Wrapup → Idle), then the agent
    // goes back on break, then off for the day.
    r.update_status(
        "a1",
        AgentStatus::Busy {
            call_id: "call-2".into(),
            since: Instant::now(),
        },
    )
    .await
    .unwrap();
    r.update_status(
        "a1",
        AgentStatus::Wrapup {
            call_id: "call-2".into(),
            since: Instant::now(),
        },
    )
    .await
    .unwrap();
    r.update_status("a1", AgentStatus::Idle).await.unwrap();
    r.update_status("a1", AgentStatus::Away("break".into()))
        .await
        .unwrap();
    r.update_status("a1", AgentStatus::Idle).await.unwrap();

    let rows = history_rows(&db, "a1").await;
    let busy_at = rows
        .iter()
        .find(|row| row.to_status == "busy")
        .map(|row| row.created_at.timestamp())
        .expect("busy row exists");

    let eps = break_episodes_from_rows(&rows, None, chrono::Utc::now());
    assert_eq!(
        eps.iter().map(|e| e.break_id.as_str()).collect::<Vec<_>>(),
        vec!["break", "break"],
        "the break must be TWO segments around the call, got {eps:?}"
    );
    // The busy window lies strictly between the two segments — never inside.
    let pre = &eps[0];
    let post = &eps[1];
    assert!(
        pre.ended_at.unwrap() <= busy_at && post.started_at >= busy_at,
        "call window must be excluded from both break segments: busy@{busy_at}, {eps:?}"
    );
    assert_disjoint_and_bounded(&eps, 300);
}

/// CASE 3: unanswered ring ON a break — the restore policy hands the agent
/// back their pre-ring break (`resumed = true`), so the episode continues.
/// (The automatic 998 cooldown state itself canNOT be forced through the
/// public API BY DESIGN — `permits_manual_update` rejects a manual
/// `Away("998")`, the transition silently no-ops — and its row shape plus
/// the endpoint view contract are covered at HTTP level in
/// `agent_status_api_tests::break_records_query_persisted_status_history`.)
/// Records invariants: the ring gap splits the episode into two bounded,
/// disjoint halves — never one swallowed mega-record.
#[tokio::test]
async fn e2e_unanswered_ring_on_break_restores_break_records_stay_bounded() {
    let db = setup_db().await;
    let r = Arc::new(AgentRegistry::with_db(db.clone()));
    r.register("a1".into(), vec![], 1).await.unwrap();
    r.update_status("a1", AgentStatus::Idle).await.unwrap();

    r.update_status("a1", AgentStatus::Away("break".into()))
        .await
        .unwrap();
    assert!(r.try_reserve_agent("a1", "call-3".into()).await);
    // The ring captured the pre-ring status; the ring terminal (no answer)
    // hands the agent THEIR status back instead of dropping to Idle.
    assert!(
        r.capture_resume_status("a1", "call-3", ResumeTarget::Away("break".into()))
            .await
    );
    assert!(r.ring_no_answer_to_idle("a1", "call-3").await.unwrap());
    let agent = r.get_agent("a1").await.unwrap();
    assert!(
        matches!(&agent.status, AgentStatus::Away(d) if d == "break"),
        "restore policy must put the agent back on break, got {}",
        agent.status
    );
    r.update_status("a1", AgentStatus::Idle).await.unwrap();

    let rows = history_rows(&db, "a1").await;
    let eps = break_episodes_from_rows(&rows, None, chrono::Utc::now());
    let ids: Vec<&str> = eps.iter().map(|e| e.break_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["break", "break"],
        "ring gap splits the episode; both halves attributed to the break: {eps:?}"
    );
    assert_disjoint_and_bounded(&eps, 300);
}
