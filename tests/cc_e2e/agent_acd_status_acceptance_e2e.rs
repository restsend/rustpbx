//! Agent / ACD status acceptance e2e (in-process, strong assertions).
//!
//! Acceptance gate for the P0/P1 fixes in the Agent/ACD Status Review:
//! each case asserts **observable correct behavior** (status, URI, metrics
//! counters, DB rows) — not "events were non-empty" tautologies.
//!
//! SIP-level wait-retention coverage lives in
//! `tests/queue_e2e/test_queue_wait_retention_e2e.rs`.

use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine, AcdPolicy, PresenceStateKind, StrategyConfig};
use rustpbx::addons::cc::agent::{AgentRegistry, AgentStatus};
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::metrics::MetricsCollector;
use rustpbx::addons::cc::models::cc_queue_status;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::app::agent_registry::AgentRegistry as TraitAgentRegistry;
use sea_orm::{ActiveModelTrait, ColumnTrait, Database, EntityTrait, QueryFilter, Set};
use sea_orm_migration::MigratorTrait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn setup_db() -> sea_orm::DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    db
}

async fn create_sg_with_policy(
    db: &sea_orm::DatabaseConnection,
    skill_group_id: &str,
    skills: Vec<String>,
    policy_name: &str,
) {
    rustpbx::addons::cc::skill_group::create_skill_group(
        db,
        CreateSkillGroupRequest {
            skill_group_id: skill_group_id.to_string(),
            display_name: Some(skill_group_id.to_string()),
            skills_required: skills,
            overflow_groups: vec![],
            sla_target_secs: 20,
            max_wait_secs: 60,
            metadata: None,
        },
    )
    .await
    .unwrap();
    let sg = rustpbx::addons::cc::skill_group::get_skill_group(db, skill_group_id)
        .await
        .unwrap()
        .unwrap();
    let mut active: rustpbx::addons::cc::models::cc_skill_group::ActiveModel = sg.into();
    active.acd_policy = Set(Some(policy_name.to_string()));
    active.update(db).await.unwrap();
}

fn engine_with_policies(policies: HashMap<String, AcdPolicy>, default: &str) -> Arc<AcdEngine> {
    Arc::new(AcdEngine::new(AcdConfig {
        enabled: true,
        policies,
        default_policy: default.to_string(),
    }))
}

/// ACCEPT: `require_exact_skill=false` must dial an agent matching ANY skill.
#[tokio::test]
async fn acceptance_partial_skill_match_dials_when_require_exact_false() {
    let db = setup_db().await;
    create_sg_with_policy(
        &db,
        "sg-any",
        vec!["support".into(), "english".into()],
        "any-skill",
    )
    .await;

    let mut policies = HashMap::new();
    policies.insert(
        "any-skill".into(),
        AcdPolicy {
            name: "any-skill".into(),
            strategy: StrategyConfig {
                require_exact_skill: false,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let registry = Arc::new(AgentRegistry::with_db(db));
    registry
        .register("a-partial".into(), vec!["support".into()], 1)
        .await
        .unwrap();
    registry
        .update_status("a-partial", AgentStatus::Idle)
        .await
        .unwrap();

    let adapter = CcAgentRegistryAdapter::new(registry.clone(), engine_with_policies(policies, "any-skill"), "localhost");
    let uris = adapter
        .resolve_target_with_policy("skill-group:sg-any", None, "call-any")
        .await;

    assert_eq!(
        uris,
        vec!["sip:a-partial@localhost".to_string()],
        "partial skill match must produce a dial URI, got {uris:?}"
    );
    let agent = registry.get_agent("a-partial").await.unwrap();
    assert!(
        matches!(
            agent.status,
            AgentStatus::Ringing {
                ref call_id,
                ..
            } if call_id == "call-any"
        ),
        "reserved agent must be Ringing(call-any), got {}",
        agent.status
    );
}

/// ACCEPT: Away is dialable when policy `available_states` includes Away.
#[tokio::test]
async fn acceptance_away_agent_reserved_when_available_states_include_away() {
    let db = setup_db().await;
    create_sg_with_policy(&db, "sg-away", vec!["support".into()], "force-away").await;

    let mut policies = HashMap::new();
    policies.insert(
        "force-away".into(),
        AcdPolicy {
            name: "force-away".into(),
            available_states: vec![PresenceStateKind::Idle, PresenceStateKind::Away],
            ..Default::default()
        },
    );
    let registry = Arc::new(AgentRegistry::with_db(db));
    registry
        .register("a-away".into(), vec!["support".into()], 1)
        .await
        .unwrap();
    registry
        .update_status("a-away", AgentStatus::Away("lunch".into()))
        .await
        .unwrap();

    let adapter =
        CcAgentRegistryAdapter::new(registry.clone(), engine_with_policies(policies, "force-away"), "localhost");
    let uris = adapter
        .resolve_target_with_policy("skill-group:sg-away", None, "call-away")
        .await;

    assert_eq!(uris, vec!["sip:a-away@localhost".to_string()]);
    let agent = registry.get_agent("a-away").await.unwrap();
    assert!(
        matches!(agent.status, AgentStatus::Ringing { .. }),
        "Away→Ringing required, got {}",
        agent.status
    );
}

/// ACCEPT: CallQueued / CallAbandoned update MetricsCollector **and**
/// `cc_queue_status.calls_waiting` (not just RWI events).
#[tokio::test]
async fn acceptance_queue_waiting_and_abandoned_bridge_metrics_and_db() {
    let db = setup_db().await;
    let metrics = Arc::new(MetricsCollector::new());
    let registry = Arc::new(AgentRegistry::with_db(db.clone()));
    let adapter = CcAgentRegistryAdapter::new(
        registry,
        Arc::new(AcdEngine::new(AcdConfig::default())),
        "localhost",
    )
    .with_metrics(metrics.clone());

    // Drive the private bridge via trait notifications + public emit path
    // used by resolve when no candidates exist.
    create_sg_with_policy(&db, "sg-wait", vec!["nosuch".into()], "default").await;
    let uris = adapter
        .resolve_target_with_policy("skill-group:sg-wait", None, "call-wait-1")
        .await;
    assert!(
        uris.is_empty(),
        "no matching agent → empty dial list (wait retention), got {uris:?}"
    );

    // Spawned metrics/DB writers need a tick.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let m = metrics
        .get_metrics("sg-wait")
        .await
        .expect("CallQueued must create MetricsCollector row");
    assert_eq!(
        m.current_waiting, 1,
        "waiting gauge must be 1 after CallQueued, got {}",
        m.current_waiting
    );
    assert!(
        m.calls_offered >= 1,
        "offered must increment on enqueue, got {}",
        m.calls_offered
    );

    let row = cc_queue_status::Entity::find()
        .filter(cc_queue_status::Column::QueueId.eq("sg-wait"))
        .one(&db)
        .await
        .unwrap()
        .expect("cc_queue_status row must exist after CallQueued");
    assert_eq!(
        row.calls_waiting, 1,
        "DB calls_waiting must be 1, got {}",
        row.calls_waiting
    );

    adapter
        .notify_call_abandoned("call-wait-1", "sg-wait", 9)
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let m = metrics.get_metrics("sg-wait").await.unwrap();
    assert_eq!(
        m.current_waiting, 0,
        "abandon must clear waiting gauge, got {}",
        m.current_waiting
    );
    assert_eq!(
        m.calls_abandoned, 1,
        "abandon counter must be 1, got {}",
        m.calls_abandoned
    );

    let row = cc_queue_status::Entity::find()
        .filter(cc_queue_status::Column::QueueId.eq("sg-wait"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.calls_waiting, 0,
        "DB calls_waiting must return to 0 after abandon"
    );
}

/// ACCEPT: ending call-B must not Wrapup an agent Busy on call-A.
#[tokio::test]
async fn acceptance_hangup_ignores_busy_bound_to_other_call() {
    use rustpbx::addons::cc::cc_call_session_hook::CcCallSessionHook;
    use rustpbx::callrecord::CallRecordHangupReason;
    use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};

    let registry = Arc::new(AgentRegistry::new());
    registry
        .register("agent-bound".into(), vec!["support".into()], 1)
        .await
        .unwrap();
    registry
        .update_status("agent-bound", AgentStatus::Idle)
        .await
        .unwrap();
    registry
        .update_status(
            "agent-bound",
            AgentStatus::Busy {
                call_id: "call-A".into(),
                since: Instant::now(),
            },
        )
        .await
        .unwrap();

    let hook = CcCallSessionHook::new(registry.clone(), Arc::new(MetricsCollector::new()));
    let ctx = CallSessionContext {
        session_id: "call-B".into(),
        root_session_id: None,
        caller: "sip:alice@localhost".into(),
        callee: "sip:agent-bound@localhost".into(),
        connected_callee: Some("sip:agent-bound@localhost".into()),
        queue_name: Some("support".into()),
        skill_group_id: None,
        transferred: false,
        direction: "inbound".into(),
        started_at: None,
        extensions: Default::default(),
    };
    {
        let mut ext = ctx.extensions.write();
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("resolved_agent_id".into(), "agent-bound".into());
        ext.insert(map);
    }

    hook.on_call_ended(&ctx, Some(&CallRecordHangupReason::ByCallee), 3)
        .await;

    let agent = registry.get_agent("agent-bound").await.unwrap();
    assert!(
        matches!(
            agent.status,
            AgentStatus::Busy {
                ref call_id,
                ..
            } if call_id == "call-A"
        ),
        "Busy(call-A) must survive ended(call-B), got {}",
        agent.status
    );
}

/// ACCEPT: stale Busy presence (>120s) must survive reap.
#[tokio::test]
async fn acceptance_reap_keeps_stale_busy_presence() {
    let db = setup_db().await;
    rustpbx::addons::cc::stats_writer::upsert_agent_presence(
        &db,
        "busy-long",
        "busy",
        &[],
        &HashMap::new(),
        1,
        1,
        0,
        chrono::Utc::now().timestamp_millis(),
    )
    .await;

    use rustpbx::addons::cc::models::cc_agent_presence;
    let record = cc_agent_presence::Entity::find()
        .filter(cc_agent_presence::Column::AgentId.eq("busy-long"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut active: cc_agent_presence::ActiveModel = record.into();
    active.updated_at = Set(chrono::Utc::now() - chrono::Duration::seconds(300));
    active.update(&db).await.unwrap();

    rustpbx::addons::cc::stats_writer::reap_stale_presence(&db).await;

    let still = cc_agent_presence::Entity::find()
        .filter(cc_agent_presence::Column::AgentId.eq("busy-long"))
        .one(&db)
        .await
        .unwrap();
    assert!(
        still.is_some(),
        "Busy presence aged 300s must NOT be reaped (long-call safety)"
    );
    assert_eq!(still.unwrap().status, "busy");
}

/// ACCEPT: auto_offline_after_wrapup forces Offline on Ringing→Idle.
#[tokio::test]
async fn acceptance_auto_offline_on_ringing_to_idle() {
    let registry = Arc::new(AgentRegistry::new());
    registry.register("ao".into(), vec![], 1).await.unwrap();
    registry
        .update_status("ao", AgentStatus::Idle)
        .await
        .unwrap();
    assert!(registry.try_reserve_agent("ao", "c1".into()).await);
    registry
        .set_auto_offline_after_wrapup("ao", true)
        .await
        .unwrap();
    registry
        .update_status("ao", AgentStatus::Idle)
        .await
        .unwrap();
    let agent = registry.get_agent("ao").await.unwrap();
    assert!(
        matches!(agent.status, AgentStatus::Offline),
        "Ringing→Idle with auto_offline must become Offline, got {}",
        agent.status
    );
}

/// ACCEPT: wrapup expiry must not yank a newer Ringing reservation.
#[tokio::test]
async fn acceptance_expire_wrapup_does_not_idle_new_ringing() {
    let registry = Arc::new(AgentRegistry::new());
    registry.register("wex".into(), vec![], 1).await.unwrap();
    registry
        .update_status("wex", AgentStatus::Idle)
        .await
        .unwrap();
    for s in [
        AgentStatus::Ringing {
            call_id: "c-old".into(),
            since: Instant::now(),
        },
        AgentStatus::Busy {
            call_id: "c-old".into(),
            since: Instant::now(),
        },
        AgentStatus::Wrapup {
            call_id: "c-old".into(),
            since: Instant::now(),
        },
    ] {
        registry.update_status("wex", s).await.unwrap();
    }
    // Leave wrapup for a new call: Idle then reserve (mirrors production).
    registry
        .update_status("wex", AgentStatus::Idle)
        .await
        .unwrap();
    assert!(registry.try_reserve_agent("wex", "c-new".into()).await);

    let expired = registry
        .expire_wrapup("wex", Some("c-old"))
        .await
        .unwrap();
    assert!(!expired, "expire_wrapup for old call must be a no-op");
    let agent = registry.get_agent("wex").await.unwrap();
    assert!(
        matches!(
            agent.status,
            AgentStatus::Ringing {
                ref call_id,
                ..
            } if call_id == "c-new"
        ),
        "new Ringing must survive old wrapup expiry, got {}",
        agent.status
    );
}

/// ACCEPT: cancel-style abandon path still reports CallAbandoned once metrics
/// were previously queued (counter integrity).
#[tokio::test]
async fn acceptance_abandon_after_queue_decrements_waiting_exactly_once() {
    let db = setup_db().await;
    let metrics = Arc::new(MetricsCollector::new());
    let registry = Arc::new(AgentRegistry::with_db(db.clone()));
    let adapter = CcAgentRegistryAdapter::new(
        registry,
        Arc::new(AcdEngine::new(AcdConfig::default())),
        "localhost",
    )
    .with_metrics(metrics.clone());
    create_sg_with_policy(&db, "sg-once", vec!["zzz".into()], "default").await;
    let _ = adapter
        .resolve_target_with_policy("skill-group:sg-once", None, "call-once")
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    adapter
        .notify_call_abandoned("call-once", "sg-once", 1)
        .await;
    // Duplicate abandon must not drive waiting negative.
    adapter
        .notify_call_abandoned("call-once", "sg-once", 1)
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let m = metrics.get_metrics("sg-once").await.unwrap();
    assert_eq!(m.current_waiting, 0, "waiting must stay non-negative at 0");
    assert!(
        m.calls_abandoned >= 1,
        "at least one abandoned recorded, got {}",
        m.calls_abandoned
    );
}
