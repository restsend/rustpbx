//! CC monitor agent-status e2e — the monitor must render LIVE presence,
//! never a stale replica.
//!
//! Regression gate for prd 2026-10-14 (agent sat on 小休 while the monitor
//! kept showing `idle`): `build_monitor_snapshot` merges the local registry
//! copy with the shared `cc_agent_presence` row by VERSION. The contracts
//! pinned here:
//!
//! 1. every registry transition is visible in the very next snapshot —
//!    status, break detail, and the `summary.agents_by_status` counts;
//! 2. a shared presence row strictly NEWER than the local copy wins for
//!    non-runtime states — a stale local `idle` must never mask a
//!    cluster-wide break (the multi-node regression: the desk moved the
//!    agent via another node, this node's replica stayed behind);
//! 3. a fresh LOCAL runtime state (`ringing`, carrying the live call
//!    binding a presence row cannot express) survives an older shared row.
//!
//! Run with the rest of the addon e2e suite:
//! `cargo test --test e2e_addons --features addon-cc`.

use rustpbx::addons::cc::agent::{AgentRegistry, AgentStatus};
use rustpbx::addons::cc::console_monitor::{build_monitor_snapshot, MonitorParams};
use rustpbx::addons::cc::stats_writer::upsert_agent_presence;
use rustpbx::addons::cc::CcAddonState;
use rustpbx::config::ConsoleConfig;
use rustpbx::console::ConsoleState;
use sea_orm::{ActiveModelTrait, EntityTrait};
use sea_orm_migration::MigratorTrait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

async fn setup() -> (Arc<ConsoleState>, Arc<AgentRegistry>) {
    let db = sea_orm::Database::connect("sqlite::memory:")
        .await
        .expect("connect sqlite memory");
    rustpbx::models::migration::Migrator::up(&db, None)
        .await
        .expect("core migrations");
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .expect("cc migrations");
    let console = ConsoleState::initialize(db.clone(), ConsoleConfig::default(), None)
        .await
        .expect("initialize console state");
    let registry = Arc::new(AgentRegistry::with_db(db.clone()));
    let mut cc_state = CcAddonState::with_db(db);
    cc_state.agent_registry = registry.clone();
    console.insert_addon_state(cc_state);
    (console, registry)
}

/// The monitor's row for one agent (`None` = not listed at all).
fn agent_view<'a>(snap: &'a Value, agent_id: &str) -> &'a Value {
    snap["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .find(|a| a["agent_id"] == agent_id)
        .unwrap_or_else(|| panic!("agent {agent_id} must be listed: {snap}"))
}

fn status_count(snap: &Value, status: &str) -> i64 {
    snap["summary"]["agents_by_status"][status]
        .as_i64()
        .unwrap_or(0)
}

/// CONTRACT 1: transitions show up in the next snapshot — the monitor must
/// never keep rendering a status the agent has already left.
#[tokio::test]
async fn monitor_reflects_break_transitions_immediately() {
    let (console, registry) = setup().await;
    registry.register("a1".into(), vec![], 1).await.unwrap();
    registry
        .update_status("a1", AgentStatus::Idle)
        .await
        .unwrap();

    let snap = build_monitor_snapshot(&console, MonitorParams::default()).await;
    assert_eq!(agent_view(&snap, "a1")["status"], "idle");
    assert_eq!(status_count(&snap, "idle"), 1);

    // 坐席进入小休 —— monitor 必须立即反映 away + 小休原因。
    registry
        .update_status("a1", AgentStatus::Away("lunch".into()))
        .await
        .unwrap();
    let snap = build_monitor_snapshot(&console, MonitorParams::default()).await;
    let a1 = agent_view(&snap, "a1");
    assert_eq!(a1["status"], "away", "break must show as away: {a1}");
    assert_eq!(a1["status_detail"], "lunch", "reason detail surfaced: {a1}");
    assert_eq!(status_count(&snap, "idle"), 0, "idle count must drop: {snap}");
    assert_eq!(status_count(&snap, "away"), 1);

    // 回到就绪。
    registry.update_status("a1", AgentStatus::Idle).await.unwrap();
    let snap = build_monitor_snapshot(&console, MonitorParams::default()).await;
    assert_eq!(agent_view(&snap, "a1")["status"], "idle");
    assert_eq!(status_count(&snap, "away"), 0);
}

/// CONTRACT 2 (the reported regression): a shared presence row strictly
/// NEWER than the local registry copy wins — a stale local `idle` must
/// never mask a cluster-wide break. (The desk moved the agent via another
/// node; this node's replica is behind. The row is the cluster contract, so
/// the test bumps it the way the owning node's write would.)
#[tokio::test]
async fn monitor_stale_local_idle_never_masks_fresher_shared_break() {
    let (console, registry) = setup().await;
    registry.register("a1".into(), vec![], 1).await.unwrap();
    registry
        .update_status("a1", AgentStatus::Idle)
        .await
        .unwrap();

    // The local copy now says idle @T. Another node moves the agent on:
    // shared row becomes away @T+5s (strictly newer version).
    let newer = chrono::Utc::now().timestamp_millis() + 5000;
    assert!(upsert_agent_presence(
        console.db(),
        "a1",
        "away",
        &[],
        &HashMap::new(),
        1,
        0,
        0,
        newer,
    )
    .await);

    let snap = build_monitor_snapshot(&console, MonitorParams::default()).await;
    let a1 = agent_view(&snap, "a1");
    assert_eq!(
        a1["status"], "away",
        "fresher shared row must beat the stale local idle: {a1}"
    );
    assert_eq!(status_count(&snap, "away"), 1);
    assert_eq!(status_count(&snap, "idle"), 0);
}

/// CONTRACT 3: a fresh LOCAL runtime state carries the live call binding a
/// presence row cannot express — an older shared row must not strip it.
#[tokio::test]
async fn monitor_keeps_fresh_local_runtime_over_older_shared_row() {
    let (console, registry) = setup().await;
    registry.register("a1".into(), vec![], 1).await.unwrap();
    registry
        .update_status("a1", AgentStatus::Idle)
        .await
        .unwrap();

    // Agent gets reserved (Idle → Ringing, local revision bumped to NOW).
    assert!(registry.try_reserve_agent("a1", "call-9".into()).await);

    // Simulate a LAGGING shared row (the peer's write is older than the
    // local runtime transition — e.g. the owner's presence write was lost).
    // `upsert_agent_presence` rejects stale versions BY DESIGN (cluster
    // write policy), so rewind the row directly: status away @ -5s.
    let db = console.db();
    let row = rustpbx::addons::cc::models::cc_agent_presence::Entity::find_by_id("a1")
        .one(db)
        .await
        .expect("query presence")
        .expect("presence row exists");
    let mut stale: rustpbx::addons::cc::models::cc_agent_presence::ActiveModel = row.into();
    use sea_orm::Set;
    stale.status = Set("away".to_string());
    stale.updated_at = Set(chrono::Utc::now() - chrono::Duration::seconds(5));
    stale.update(db).await.expect("rewind shared row");

    let snap = build_monitor_snapshot(&console, MonitorParams::default()).await;
    let a1 = agent_view(&snap, "a1");
    assert_eq!(
        a1["status"], "ringing",
        "fresh local runtime state survives an older row: {a1}"
    );
    assert_eq!(
        a1["current_call_id"], "call-9",
        "live call binding surfaced from the local runtime: {a1}"
    );
}
