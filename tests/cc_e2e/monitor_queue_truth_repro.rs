//! 排队计数一致性回归（原 REPRO，修复后转绿）。
//!
//! 系统里「排队」只能有一个定义：`cc_acd_queue` 里 `claimed_by IS NULL` 的行。
//! claimed（正在坐席振铃）= 振铃中，不计入排队 —— monitor、mini 条
//! (`/cc/metrics`)、Prometheus、单队列 metrics、`agents_exhausted` 全部同源。
//!
//! ① 振铃期：monitor 排队中 / metrics 排队 / 单队列 metrics 三处一致（都
//!    不含振铃中的那通）。
//! ② 整个集群：两节点共享一个集群库，入队走生产写路径（adapter 同款
//!    `shared_enqueue`）后，对端节点 monitor 与坐席端排队视图看到同一通
//!    通话 —— monitor 展示的就是整个集群。

use std::sync::Arc;

use rustpbx::addons::cc::console_monitor::{MonitorParams, build_monitor_snapshot};
use rustpbx::addons::cc::stats_writer::{
    count_shared_queue_for_group, real_waiting_counts, shared_enqueue,
};
use rustpbx::console::ConsoleState;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait as _;

/// 一个共享的「集群库」，只跑 cc 迁移（repro 只读 cc_* 表）。
async fn migrated_shared_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("sqlite memory");
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .expect("cc migrations");
    db
}

async fn node(db: &DatabaseConnection) -> Arc<ConsoleState> {
    ConsoleState::initialize(db.clone(), rustpbx::config::ConsoleConfig::default(), None)
        .await
        .expect("console state")
}

async fn seed_skill_group(db: &DatabaseConnection, id: &str) {
    rustpbx::addons::cc::models::cc_skill_group::ActiveModel {
        skill_group_id: Set(id.to_string()),
        skills_required: Set(serde_json::json!([])),
        overflow_groups: Set(serde_json::json!([])),
        sla_target_secs: Set(30),
        max_wait_secs: Set(90),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed skill group");
}

/// ① 振铃中不计入排队 —— monitor / metrics / 单队列 metrics 三处一致。
#[tokio::test]
async fn ringing_call_is_not_counted_as_waiting_on_any_read_path() {
    use rustpbx::addons::cc::models::cc_acd_queue;

    let db = migrated_shared_db().await;
    let state = node(&db).await;
    seed_skill_group(&db, "support").await;

    let now = chrono::Utc::now();
    // 共享队列行；坐席已收到 offer → claimed（振铃中）。
    cc_acd_queue::ActiveModel {
        call_id: Set("sess-ringing".into()),
        queue_id: Set("support".into()),
        trace_id: Set("t".into()),
        caller_number: Set("138".into()),
        required_skills: Set(serde_json::json!([])),
        priority: Set(0),
        enqueued_by: Set(Some("node-a".into())),
        claimed_by: Set(Some("node-a".into())),
        claimed_at: Set(Some(now)),
        enqueued_at: Set(now - chrono::Duration::seconds(6)),
    }
    .insert(&db)
    .await
    .expect("seed shared queue row");

    let snap = build_monitor_snapshot(&state, MonitorParams::default()).await;
    let monitor_waiting = snap["summary"]["waiting"].as_u64().unwrap();
    // mini 条 / `/cc/metrics` 的排队来源。
    let metrics_waiting = real_waiting_counts(&db)
        .await
        .get("support")
        .copied()
        .unwrap_or(0) as u64;
    // 单队列 metrics（`GET /cc/skill-groups/{id}/metrics`）的排队来源。
    let single_queue_waiting = count_shared_queue_for_group(&db, "support")
        .await
        .unwrap_or(-1) as u64;

    assert_eq!(monitor_waiting, 0, "monitor: claimed 归『振铃中』");
    assert_eq!(
        snap["summary"]["dispatching"], 1,
        "monitor: claimed = 振铃中"
    );
    assert_eq!(
        metrics_waiting, 0,
        "mini/metrics: 振铃中不计入排队（曾按含 claimed 计为 1）"
    );
    assert_eq!(single_queue_waiting, 0, "单队列 metrics 与集群计数同口径");
    assert_eq!(
        metrics_waiting, monitor_waiting,
        "同一系统对『排队』只有一个数"
    );
}

/// ② 整个集群：入队即写共享行，对端 monitor 与坐席端排队视图同源同值。
#[tokio::test]
async fn queue_entry_is_cluster_visible_to_monitor_and_agent_views() {
    let db = migrated_shared_db().await;
    seed_skill_group(&db, "support").await;

    // 两节点共享同一集群库（生产为共享 MySQL）。
    let node_a = node(&db).await;
    let node_b = node(&db).await;

    // 生产入队写路径 —— 与 agent_registry_adapter 的 queue-app 相同的
    // `shared_enqueue`（幂等 upsert，同步 await）。`enqueued_by` 由
    // `shared_enqueue` 强制写入本节点 instance id（集群对账/回收依据）。
    shared_enqueue(&db, "sess-cluster-1", "support", "t", "13800001234", &[], 0).await;

    let snap_a = build_monitor_snapshot(&node_a, MonitorParams::default()).await;
    let snap_b = build_monitor_snapshot(&node_b, MonitorParams::default()).await;
    let a_waiting = snap_a["summary"]["waiting"].as_u64().unwrap();
    let b_waiting = snap_b["summary"]["waiting"].as_u64().unwrap();

    // 坐席端（mini 条 / `/cc/metrics`）的排队视图 —— 集群共享表。
    let agent_view = real_waiting_counts(&db)
        .await
        .get("support")
        .copied()
        .unwrap_or(0) as u64;

    assert_eq!(a_waiting, 1, "节点 A monitor 看到这通排队");
    assert_eq!(b_waiting, 1, "对端节点 B monitor 必须看到同一通排队");
    assert_eq!(agent_view, 1, "坐席端排队视图 = 整个集群（不是本节点本地）");
    assert_eq!(agent_view as u64, a_waiting, "坐席端与 monitor 排队一致");

    // 行必须带正确的入队节点归属（集群对账/回收依据）。
    let rows = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].call_id, "sess-cluster-1");
    assert_eq!(
        rows[0].enqueued_by.as_deref(),
        Some(rustpbx::addons::cc::stats_writer::get_instance_id().as_str()),
        "enqueued_by = 本节点 instance id"
    );
}
