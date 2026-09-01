//! End-to-end cluster simulation tests.
//!
//! These tests simulate a **two-node cluster** sharing a single SQLite
//! in-memory database.  They verify:
//!
//! 1. Agent status persistence (`cc_agent_presence`)
//! 2. Shared distributed queue (`cc_acd_queue`) — enqueue, claim, dequeue
//! 3. Cluster event message serialization (agent_status + queue_event)
//! 4. Cross-node agent visibility (node A agents visible to node B's tick)
//! 5. **Affinity**: peers do NOT steal live calls from a healthy enqueue owner
//! 6. Failover cleanup when enqueue owner is dead
//! 7. Reaper **releases** stale claims (does not delete waiting calls)
//! 8. Concurrent claim race — only one node wins
//! 9. Cluster-wide longest-idle selection across merged agent snapshots

use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

/// Create an in-memory SQLite database with ALL migrations applied
/// (core + CC addon).  This simulates the shared DB in a cluster.
async fn shared_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::models::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    db
}

/// `RUSTPBX_INSTANCE_ID` is process-global; tests that mutate it (and the
/// cached INSTANCE_ID) must run serially or they cross-contaminate node
/// identity under the parallel test harness.
static INSTANCE_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn instance_env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    INSTANCE_ENV_LOCK.lock().await
}

// ═══════════════════════════════════════════════════════════════════
// Test 1: Agent status written by node A is visible to node B
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_agent_status_cross_node_visibility() {
    let db = shared_db().await;

    // Node A registers an agent and persists its status.
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "agent-on-node-a",
        "idle",
        &["support".to_string(), "english".to_string()],
        &std::collections::HashMap::from([("support".to_string(), 7)]),
        2,
        0,
        15,
        1,
    )
    .await;

    // Node B reads ALL agent presence — should see node A's agent.
    let all = rustpbx::addons::cc::stats_writer::read_all_agent_presence(&db).await;
    assert_eq!(all.len(), 1, "node B should see node A's agent");

    let agent = &all[0];
    assert_eq!(agent.agent_id, "agent-on-node-a");
    assert_eq!(agent.status, "idle");
    assert!(agent.skills.contains(&"support".to_string()));
    assert_eq!(agent.skill_levels.get("support"), Some(&7));
    assert_eq!(agent.max_concurrency, 2);
    assert_eq!(agent.priority, 15);

    // Node B can build an AgentSnapshot from this DB row.
    let snapshot = acd_snapshot_from_presence(agent);
    assert_eq!(snapshot.agent_id, "agent-on-node-a");
    assert_eq!(
        snapshot.presence,
        rustpbx::call::app::agent_registry::PresenceState::Idle
    );
    assert!(snapshot.skills.contains(&"support".to_string()));
}

// ═══════════════════════════════════════════════════════════════════
// Test 2: Shared queue — node A enqueues, node B claims
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_shared_queue_cross_node_claim() {
    let db = shared_db().await;

    // Node A enqueues a call.
    rustpbx::addons::cc::stats_writer::shared_enqueue(
        &db,
        "call-from-node-a",
        "sg_support",
        "trace-a",
        "1001",
        &["support".to_string()],
        10,
    )
    .await;

    // Node B reads the shared queue — sees node A's call.
    let queue = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].call_id, "call-from-node-a");
    assert_eq!(queue[0].queue_id, "sg_support");
    assert!(queue[0].required_skills.contains(&"support".to_string()));

    // Node B atomically claims the call.
    let claimed = rustpbx::addons::cc::stats_writer::shared_claim(&db, "call-from-node-a").await;
    assert!(claimed, "node B should successfully claim");

    // Node C (or node A retrying) cannot claim the same call.
    let claimed2 = rustpbx::addons::cc::stats_writer::shared_claim(&db, "call-from-node-a").await;
    assert!(!claimed2, "second claim must fail");

    // After dequeue, the call is gone from the shared queue.
    rustpbx::addons::cc::stats_writer::shared_dequeue(&db, "call-from-node-a").await;
    let remaining = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    assert!(
        remaining.is_empty(),
        "shared queue should be empty after dequeue"
    );
}

// ═══════════════════════════════════════════════════════════════════
// Test 3: Cluster event messages round-trip
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_event_messages_serialize_correctly() {
    use rustpbx::proxy::cluster_event::{ClusterAgentStatusMessage, ClusterQueueEventMessage};

    // Agent status message.
    let agent_msg = ClusterAgentStatusMessage {
        agent_id: "agent-x".into(),
        status: "idle".into(),
        skills: serde_json::json!(["billing"]),
        skill_levels: serde_json::json!({"billing": 8}),
        max_concurrency: 3,
        current_calls: 1,
        priority: 5,
        instance_id: "node-a".into(),
        revision: 1,
    };
    let json = serde_json::to_string(&agent_msg).unwrap();
    let parsed: ClusterAgentStatusMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.agent_id, "agent-x");
    assert_eq!(parsed.status, "idle");
    assert_eq!(parsed.max_concurrency, 3);

    // Queue event message (enqueue).
    let enqueue_msg = ClusterQueueEventMessage {
        action: "enqueue".into(),
        call_id: "call-y".into(),
        queue_id: "sg_sales".into(),
        trace_id: "t".into(),
        agent_id: None,
        required_skills: vec!["sales".into()],
        priority: 0,
    };
    let json = serde_json::to_string(&enqueue_msg).unwrap();
    assert!(json.contains("\"action\":\"enqueue\""));
    assert!(json.contains("\"call_id\":\"call-y\""));

    // Queue event message (assign).
    let assign_msg = ClusterQueueEventMessage {
        action: "assign".into(),
        call_id: "call-y".into(),
        queue_id: "sg_sales".into(),
        trace_id: "t".into(),
        agent_id: Some("agent-x".into()),
        required_skills: vec![],
        priority: 0,
    };
    let json = serde_json::to_string(&assign_msg).unwrap();
    assert!(json.contains("\"action\":\"assign\""));
    assert!(json.contains("\"agent_id\":\"agent-x\""));
}

// ═══════════════════════════════════════════════════════════════════
// Test 4: Multi-agent, multi-queue cluster scenario
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_multi_node_multi_agent_scenario() {
    let db = shared_db().await;

    // ── Setup: two nodes, each with agents ──────────────────────

    // Node A: 2 idle agents with "support" skill.
    for id in &["a-agent-1", "a-agent-2"] {
        rustpbx::addons::cc::stats_writer::upsert_agent_presence(
            &db,
            id,
            "idle",
            &["support".to_string()],
            &std::collections::HashMap::from([("support".to_string(), 5)]),
            1,
            0,
            0,
            1,
        )
        .await;
    }

    // Node B: 1 idle agent with "billing" skill.
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "b-agent-1",
        "idle",
        &["billing".to_string()],
        &std::collections::HashMap::from([("billing".to_string(), 5)]),
        1,
        0,
        0,
        1,
    )
    .await;

    // ── Node A enqueues a "billing" call ────────────────────────
    rustpbx::addons::cc::stats_writer::shared_enqueue(
        &db,
        "billing-call",
        "sg_billing",
        "trace-bill",
        "2001",
        &["billing".to_string()],
        0,
    )
    .await;

    // ── Node A's tick: reads all presence, tries to find local agent ──
    let all_presence = rustpbx::addons::cc::stats_writer::read_all_agent_presence(&db).await;
    assert_eq!(
        all_presence.len(),
        3,
        "should see all 3 agents cluster-wide"
    );

    // Node A has no "billing" agents — the call stays in the shared queue.
    let node_a_agents_with_billing: Vec<_> = all_presence
        .iter()
        .filter(|a| a.skills.contains(&"billing".to_string()) && a.status == "idle")
        .collect();
    // Node B's agent has "billing" — visible via DB.
    assert_eq!(node_a_agents_with_billing.len(), 1);
    assert_eq!(node_a_agents_with_billing[0].agent_id, "b-agent-1");

    // ── Verify queue counts ─────────────────────────────────────
    let count_billing =
        rustpbx::addons::cc::stats_writer::count_shared_queue(&db, "sg_billing").await;
    assert_eq!(count_billing, 1);
}

// ═══════════════════════════════════════════════════════════════════
// Test 5: Reaper releases stale claims (does not delete waiting calls)
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_reaper_releases_stale_claims_after_node_crash() {
    let db = shared_db().await;
    use rustpbx::addons::cc::models::cc_acd_queue;
    use sea_orm::ActiveModelTrait;

    // Simulate: node B claimed a call but then crashed (>60s).
    let old = chrono::Utc::now() - chrono::Duration::seconds(90);
    cc_acd_queue::ActiveModel {
        call_id: sea_orm::Set("crashed-claim".into()),
        queue_id: sea_orm::Set("sg_x".into()),
        trace_id: sea_orm::Set("t".into()),
        caller_number: sea_orm::Set("100".into()),
        required_skills: sea_orm::Set(serde_json::json!([])),
        priority: sea_orm::Set(0),
        enqueued_by: sea_orm::Set(Some("node-a".into())),
        claimed_by: sea_orm::Set(Some("node-b-crashed".into())),
        claimed_at: sea_orm::Set(Some(old)),
        enqueued_at: sea_orm::Set(old),
    }
    .insert(&db)
    .await
    .unwrap();

    // Also simulate: an agent on the crashed node still shows as "idle".
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "agent-on-crashed-node",
        "idle",
        &[],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
        1,
    )
    .await;

    // Backdate the presence timestamp.
    use rustpbx::addons::cc::models::cc_agent_presence;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let record = cc_agent_presence::Entity::find()
        .filter(cc_agent_presence::Column::AgentId.eq("agent-on-crashed-node"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut active: cc_agent_presence::ActiveModel = record.into();
    active.updated_at = sea_orm::Set(chrono::Utc::now() - chrono::Duration::seconds(150));
    active.update(&db).await.unwrap();

    // Run reaper.
    rustpbx::addons::cc::stats_writer::reap_stale_queue_rows(&db).await;
    rustpbx::addons::cc::stats_writer::reap_stale_presence(&db).await;

    // Waiting call survives; claim is released.
    let row = cc_acd_queue::Entity::find()
        .filter(cc_acd_queue::Column::CallId.eq("crashed-claim"))
        .one(&db)
        .await
        .unwrap()
        .expect("waiting call must not be deleted when claim goes stale");
    assert!(row.claimed_by.is_none());
    assert!(row.claimed_at.is_none());

    // Stale presence is gone.
    let stale_presence = cc_agent_presence::Entity::find()
        .filter(cc_agent_presence::Column::AgentId.eq("agent-on-crashed-node"))
        .one(&db)
        .await
        .unwrap();
    assert!(
        stale_presence.is_none(),
        "stale presence should be reaped after node crash"
    );
}

// ═══════════════════════════════════════════════════════════════════
// Test 6: Affinity — peer does not steal live call; dead owner cleaned
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_affinity_peer_does_not_steal_live_call() {
    let db = shared_db().await;
    let _env = instance_env_guard().await;

    unsafe { std::env::set_var("RUSTPBX_INSTANCE_ID", "node-a") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();

    rustpbx::addons::cc::stats_writer::shared_enqueue(
        &db,
        "lifecycle-call",
        "sg_support",
        "lc-trace",
        "3001",
        &["support".to_string()],
        5,
    )
    .await;

    // Keep node-a alive via presence.
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "a-support-agent",
        "idle",
        &["support".to_string()],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
        1,
    )
    .await;

    // Node B has an idle matching agent — still must not steal.
    unsafe { std::env::set_var("RUSTPBX_INSTANCE_ID", "node-b") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "b-support-agent",
        "idle",
        &["support".to_string()],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
        1,
    )
    .await;

    let cleaned =
        rustpbx::addons::cc::stats_writer::cleanup_orphaned_shared_calls(&db, "node-b").await;
    assert!(
        cleaned.is_empty(),
        "affinity: peer must not claim live call from healthy enqueue owner"
    );

    let q = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].call_id, "lifecycle-call");
    assert_eq!(q[0].enqueued_by.as_deref(), Some("node-a"));

    // Owner dequeue (normal assign path on enqueue node).
    rustpbx::addons::cc::stats_writer::shared_dequeue(&db, "lifecycle-call").await;
    assert!(
        rustpbx::addons::cc::stats_writer::read_shared_queue(&db)
            .await
            .is_empty()
    );

    unsafe { std::env::remove_var("RUSTPBX_INSTANCE_ID") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();
}

#[tokio::test]
async fn cluster_failover_cleans_dead_owner_orphan() {
    let db = shared_db().await;
    let _env = instance_env_guard().await;
    use rustpbx::addons::cc::models::cc_acd_queue;
    use sea_orm::ActiveModelTrait;

    cc_acd_queue::ActiveModel {
        call_id: sea_orm::Set("dead-owner-call".into()),
        queue_id: sea_orm::Set("sg_support".into()),
        trace_id: sea_orm::Set("t".into()),
        caller_number: sea_orm::Set("3001".into()),
        required_skills: sea_orm::Set(serde_json::json!(["support"])),
        priority: sea_orm::Set(5),
        enqueued_by: sea_orm::Set(Some("node-a-dead".into())),
        claimed_by: sea_orm::Set(None),
        claimed_at: sea_orm::Set(None),
        enqueued_at: sea_orm::Set(chrono::Utc::now() - chrono::Duration::seconds(20)),
    }
    .insert(&db)
    .await
    .unwrap();

    unsafe { std::env::set_var("RUSTPBX_INSTANCE_ID", "node-b") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();

    let cleaned =
        rustpbx::addons::cc::stats_writer::cleanup_orphaned_shared_calls(&db, "node-b").await;
    assert_eq!(cleaned.len(), 1);
    assert_eq!(cleaned[0].call_id, "dead-owner-call");
    assert!(
        rustpbx::addons::cc::stats_writer::read_shared_queue(&db)
            .await
            .is_empty()
    );

    unsafe { std::env::remove_var("RUSTPBX_INSTANCE_ID") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();
}

// ═══════════════════════════════════════════════════════════════════
// Test 7: Concurrent claim race — only one node wins
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_concurrent_claim_only_one_wins() {
    let db = shared_db().await;

    // Enqueue 3 calls.
    for i in 0..3 {
        rustpbx::addons::cc::stats_writer::shared_enqueue(
            &db,
            &format!("race-call-{}", i),
            "sg_x",
            "t",
            "100",
            &[],
            0,
        )
        .await;
    }

    // Simulate two nodes racing to claim calls concurrently.
    let db_a = db.clone();
    let db_b = db.clone();

    let handle_a = tokio::spawn(async move {
        let mut won = 0;
        for i in 0..3 {
            if rustpbx::addons::cc::stats_writer::shared_claim(&db_a, &format!("race-call-{}", i))
                .await
            {
                won += 1;
            }
        }
        won
    });

    let handle_b = tokio::spawn(async move {
        // Small delay to simulate network latency.
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        let mut won = 0;
        for i in 0..3 {
            if rustpbx::addons::cc::stats_writer::shared_claim(&db_b, &format!("race-call-{}", i))
                .await
            {
                won += 1;
            }
        }
        won
    });

    let wins_a = handle_a.await.unwrap();
    let wins_b = handle_b.await.unwrap();

    // Total claims should be exactly 3 (each call claimed exactly once).
    assert_eq!(
        wins_a + wins_b,
        3,
        "total claims should be 3, got a={}, b={}",
        wins_a,
        wins_b
    );
    // Node A (which went first) should win all 3 since it has no delay.
    assert_eq!(wins_a, 3, "node A should win all 3");
    assert_eq!(wins_b, 0, "node B should win 0");
}

// ═══════════════════════════════════════════════════════════════════
// Helper: build an AgentSnapshot from a DB presence row
// (mirrors what the tick loop does)
// ═══════════════════════════════════════════════════════════════════

fn acd_snapshot_from_presence(
    p: &rustpbx::addons::cc::stats_writer::cc_agent_presence_with_status::AgentPresenceInfo,
) -> rustpbx::addons::cc::acd::AgentSnapshot {
    use rustpbx::call::app::agent_registry::PresenceState;

    let presence = match p.status.as_str() {
        "idle" => PresenceState::Idle,
        "busy" => PresenceState::Busy { call_id: None },
        "offline" => PresenceState::Offline,
        "dnd" => PresenceState::Dnd,
        other => PresenceState::Away(other.to_string()),
    };

    rustpbx::addons::cc::acd::AgentSnapshot {
        agent_id: p.agent_id.clone(),
        display_name: String::new(),
        skills: p.skills.clone(),
        skill_levels: if p.skill_levels.is_empty() {
            p.skills.iter().map(|s| (s.clone(), 5)).collect()
        } else {
            p.skill_levels.clone()
        },
        max_concurrency: p.max_concurrency as u32,
        current_calls: p.current_calls as u32,
        presence,
        idle_duration_secs: 0,
        total_calls_handled: 0,
        priority: p.priority,
        csat_avg: None,
    }
}

#[tokio::test]
async fn cluster_merged_presence_feeds_longest_idle() {
    let db = shared_db().await;
    let _env = instance_env_guard().await;

    unsafe { std::env::set_var("RUSTPBX_INSTANCE_ID", "node-a") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "local-agent",
        "idle",
        &["support".to_string()],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
        1,
    )
    .await;

    unsafe { std::env::set_var("RUSTPBX_INSTANCE_ID", "node-b") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "remote-agent",
        "idle",
        &["support".to_string()],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
        1,
    )
    .await;

    use rustpbx::addons::cc::models::cc_agent_presence;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
    let record = cc_agent_presence::Entity::find()
        .filter(cc_agent_presence::Column::AgentId.eq("remote-agent"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut active: cc_agent_presence::ActiveModel = record.into();
    active.status_since = sea_orm::Set(chrono::Utc::now() - chrono::Duration::seconds(400));
    active.update(&db).await.unwrap();

    let all = rustpbx::addons::cc::stats_writer::read_all_agent_presence(&db).await;
    assert_eq!(all.len(), 2);

    let mut snapshots: Vec<_> = all.iter().map(acd_snapshot_from_presence).collect();
    for (snap, row) in snapshots.iter_mut().zip(all.iter()) {
        snap.idle_duration_secs =
            (chrono::Utc::now() - row.status_since).num_seconds().max(0) as u64;
    }

    use rustpbx::addons::cc::acd::config::{PresenceStateKind, StrategyConfig, StrategyType};
    use rustpbx::addons::cc::acd::strategy::select_best_agent;
    let config = StrategyConfig {
        strategy_type: StrategyType::LongestIdle,
        ..Default::default()
    };
    let mut counter = 0u64;
    let best = select_best_agent(
        &mut snapshots,
        &config,
        &mut counter,
        &["support".to_string()],
        &[PresenceStateKind::Idle],
    );
    assert_eq!(
        best.unwrap().agent_id,
        "remote-agent",
        "enqueue node must pick globally longest-idle agent from merged view"
    );

    unsafe { std::env::remove_var("RUSTPBX_INSTANCE_ID") };
    rustpbx::addons::cc::stats_writer::reset_instance_id_for_test();
}

// ═══════════════════════════════════════════════════════════════════
// Call-owner session registry: dialog alias + transfer leg contract
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_session_registry_dialog_alias_routes_to_owner() {
    use rustpbx::call::runtime::{DbSessionRegistry, SessionInfo, resolve_owner_and_session};
    use std::time::Duration;

    let db = shared_db().await;
    let reg: rustpbx::call::runtime::SessionRegistryRef =
        DbSessionRegistry::new(db, Duration::from_secs(3600)).into_ref();
    reg.register(&SessionInfo::new("sess-owner-1", "10.0.0.2:5060"))
        .await
        .unwrap();
    reg.register(&SessionInfo::dialog_alias(
        "bleg-call-id-xyz",
        "sess-owner-1",
        "10.0.0.2:5060",
    ))
    .await
    .unwrap();

    let (owner, sid) = resolve_owner_and_session(&reg, "bleg-call-id-xyz")
        .await
        .expect("dialog alias must resolve to owner");
    assert_eq!(owner, "10.0.0.2:5060");
    assert_eq!(sid, "sess-owner-1");
}

#[test]
fn cluster_console_transfer_uses_callee_leg() {
    use rustpbx::call::adapters::console_to_call_command;
    use rustpbx::call::domain::CallCommand;
    use rustpbx::console::handlers::call_control::CallCommandPayload;

    let cmd = console_to_call_command(
        CallCommandPayload::Transfer {
            target: "sip:1002@example.com".into(),
            attended: Some(false),
        },
        "sess",
    )
    .unwrap();
    match cmd {
        CallCommand::Transfer {
            leg_id, attended, ..
        } => {
            assert_eq!(leg_id.as_str(), "callee");
            assert!(!attended);
        }
        other => panic!("expected Transfer, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════
// Test 10: Cross-node logout must be visible in node B's list view
// ═══════════════════════════════════════════════════════════════════
//
// BUG REPRODUCTION: an agent logged out on node A (unregistration event),
// but node B's in-memory copy still says `idle` (the peer status broadcast
// was lost). When node B serves the CC panel list it merges its memory
// with the shared `cc_agent_presence` rows; a fresh `offline` row owned by
// node A must win over node B's stale `idle`.

#[tokio::test]
async fn cluster_remote_logout_overrides_stale_local_idle_in_list_view() {
    let _guard = instance_env_guard().await;
    let db = shared_db().await;

    // ── Earlier: the agent was registered on node A; node B synced the
    //    idle snapshot into its memory (cluster agent_status broadcast).
    let registry = rustpbx::addons::cc::agent::AgentRegistry::new();
    let synced = registry.sync_routing_state(
        "agent-logout-1",
        rustpbx::addons::cc::agent::AgentStatus::Idle,
        vec![],
        std::collections::HashMap::new(),
        1,
        0,
        0,
        1_000,
        "node-a",
    );
    assert!(synced, "node B should adopt node A's idle snapshot");

    // Node B's list-view memory map, built the same way `list_agents` does.
    let memory: std::collections::HashMap<String, (String, i32)> = registry
        .list_agents()
        .await
        .into_iter()
        .map(|a| (a.agent_id.clone(), (a.status.to_string(), a.current_calls as i32)))
        .collect();
    assert_eq!(
        memory.get("agent-logout-1").map(|(s, _)| s.as_str()),
        Some("idle"),
        "precondition: node B memory holds a stale idle copy"
    );

    // ── Now: the agent logs out on node A. Node A's stats writer records
    //    a fresh `offline` row in the shared table.
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "agent-logout-1",
        "offline",
        &[],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
        chrono::Utc::now().timestamp_millis(),
    )
    .await;
    // Retag the row as owned by node A (in production the writing node
    // stamps its own instance id; this process would stamp node B's).
    use sea_orm::{ActiveModelTrait, EntityTrait};
    let shared = rustpbx::addons::cc::models::cc_agent_presence::Entity::find_by_id(
        "agent-logout-1",
    )
    .one(&db)
    .await
    .unwrap()
    .unwrap();
    let mut node_a_row: rustpbx::addons::cc::models::cc_agent_presence::ActiveModel =
        shared.into();
    node_a_row.instance_id = sea_orm::Set(Some("node-a".to_string()));
    node_a_row.update(&db).await.unwrap();

    // ── Node B serves the panel list: merge memory with shared rows.
    let rows = rustpbx::addons::cc::stats_writer::read_all_agent_presence(&db).await;
    let merged = rustpbx::addons::cc::console_handlers::merge_agent_presence_states(
        memory,
        rows,
        chrono::Utc::now() - chrono::Duration::seconds(90),
        "node-b",
    );
    assert_eq!(
        merged.get("agent-logout-1").map(|(s, _)| s.as_str()),
        Some("offline"),
        "fresh offline row written by node A must override node B's stale idle"
    );
}

