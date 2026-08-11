//! End-to-end cluster simulation tests.
//!
//! These tests simulate a **two-node cluster** sharing a single SQLite
//! in-memory database.  They verify the complete data-flow:
//!
//! 1. Agent status persistence (`cc_agent_presence`)
//! 2. Shared distributed queue (`cc_acd_queue`) — enqueue, claim, dequeue
//! 3. Cluster event message serialization (agent_status + queue_event)
//! 4. Cross-node agent visibility (node A agents visible to node B's tick)
//! 5. Cross-node queue claim (node B claims node A's queued call)
//! 6. Reaper cleanup of stale rows
//! 7. Full lifecycle: enqueue → remote claim → local enqueue → dequeue

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
// Test 5: Reaper cleans up after node crash
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_reaper_cleans_after_node_crash() {
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

    // Stale claimed row is gone.
    let stale_row = cc_acd_queue::Entity::find()
        .filter(cc_acd_queue::Column::CallId.eq("crashed-claim"))
        .one(&db)
        .await
        .unwrap();
    assert!(stale_row.is_none(), "stale claimed row should be reaped");

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
// Test 6: Full cross-node call lifecycle
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cluster_full_cross_node_lifecycle() {
    let db = shared_db().await;

    // ── Step 1: Node A has a queued call ────────────────────────
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

    let q = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    assert_eq!(q.len(), 1, "step 1: one call in shared queue");

    // ── Step 2: Node B has an idle "support" agent ──────────────
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "b-support-agent",
        "idle",
        &["support".to_string()],
        &std::collections::HashMap::new(),
        1,
        0,
        0,
    )
    .await;

    // ── Step 3: Node B's tick finds the call and claims it ──────
    let queue = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    let call = queue
        .iter()
        .find(|c| c.call_id == "lifecycle-call")
        .unwrap();

    let presence = rustpbx::addons::cc::stats_writer::read_all_agent_presence(&db).await;
    let agent = presence
        .iter()
        .find(|a| a.status == "idle" && a.skills.contains(&"support".to_string()))
        .unwrap();

    // Skill match check.
    assert!(
        call.required_skills
            .iter()
            .all(|s| agent.skills.contains(s)),
        "step 3: agent skills match call requirements"
    );

    // Claim succeeds.
    let claimed = rustpbx::addons::cc::stats_writer::shared_claim(&db, &call.call_id).await;
    assert!(claimed, "step 3: claim should succeed");

    // ── Step 4: Node B dequeues (enqueued locally) ──────────────
    rustpbx::addons::cc::stats_writer::shared_dequeue(&db, &call.call_id).await;

    let remaining = rustpbx::addons::cc::stats_writer::read_shared_queue(&db).await;
    assert!(
        remaining.is_empty(),
        "step 4: shared queue should be empty after dequeue"
    );

    // ── Step 5: Verify no orphan rows remain ────────────────────
    let count = rustpbx::addons::cc::stats_writer::count_shared_queue(&db, "sg_support").await;
    assert_eq!(count, 0, "step 5: no orphan rows in sg_support");
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
