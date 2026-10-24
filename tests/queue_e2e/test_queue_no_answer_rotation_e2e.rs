//! No-answer ROTATION contract e2e (per-agent requeue, Option-A dequeue):
//!
//! 排队 → alice 振铃 → **超时无应答** → 重新入队 → **轮转到 bob**（不再骚扰
//! alice）→ bob 接听 → 离开队列.
//!
//! Two agents in one skill group; alice registers/goes Idle FIRST so the
//! longest-idle strategy makes her the round-1 candidate. Her phone rings
//! but she never picks up; the per-agent ring timeout CANCELs her leg and
//! the call is re-queued. The next wait-retention poll MUST offer a FRESH
//! agent — bob — never alice again (single-agent-per-offer rotation).
//!
//! RWI webhook contract under test:
//!
//! 1. `queue_joined` — caller queued (attempt absent)
//! 2. `queue_agent_offered {alice}` — round 1 rings
//! 3. `queue_agent_no_answer {alice, attempt:1}` — ring timeout
//! 4. `queue_joined {attempt:2}` — the per-agent re-entry
//! 5. `queue_agent_offered {bob}` — round 2 goes to the OTHER agent
//!    (**no second `queue_agent_offered {alice}`** — the no-answered agent
//!    is excluded until a full rotation completes)
//! 6. `queue_agent_connected {bob}` — bob answers
//! 7. `queue_left {reason:"connected"}` — the ONLY `queue_left`
//! 8. `call_hangup`

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::{CcAgentRegistryAdapter, SkillGroupEvent};
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::addons::cc::translate_skill_group_event;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueFallbackConfig,
    RouteQueueStrategyConfig, RouteQueueTargetConfig, RouteRule,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rwi_timeline::RwiTimeline;
use crate::common::test_ua::{
    TestUa, TestUaConfig, TestUaEvent, create_test_sdp, create_test_sdp_answer,
};
use crate::common::webhook_capture::WebhookCapture;

const QUEUE_NUMBER: &str = "9301";
const QUEUE_NAME: &str = "rotation_q";
const SKILL_GROUP: &str = "sg_no_answer_rotation";
/// Per-agent ring timeout requested via the route (`wait_timeout_secs`).
/// NOTE: with a CC registry the desk policy `ring_no_answer_secs` (30s)
/// takes precedence over the route value (commit 6d259026), so the test
/// windows below budget the ACTUAL 30s server timer, not this constant.
const RING_TIMEOUT_SECS: u64 = 2;
/// Effective per-agent ring timeout in this harness (CC desk policy).
const EFFECTIVE_RING_TIMEOUT_SECS: u64 = 30;

/// Every RWI event type this scenario is expected to produce.
const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_no_answer",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_joined",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "skill_group_agent_no_answer",
    "skill_group_agent_rejected",
    "skill_group_position_changed",
    "skill_group_agent_connected",
    "skill_group_call_left",
];

/// Closed set for [`RwiTimeline::assert_only_expected`].
const ALLOWED_EVENT_TYPES: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_progress",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_no_answer",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_joined",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "skill_group_no_agent",
    "skill_group_agent_no_answer",
    "skill_group_agent_connected",
    "skill_group_call_left",
    "skill_group_position_changed",
    "skill_group_agent_rejected",
];

fn proxy_config() -> ProxyConfig {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };

    let queue_config = RouteQueueConfig {
        name: Some(QUEUE_NAME.to_string()),
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Sequential,
            // Maps to the per-agent ring timeout in the queue plan.
            wait_timeout_secs: Some(RING_TIMEOUT_SECS as u16),
            targets: vec![RouteQueueTargetConfig {
                uri: format!("skill-group:{SKILL_GROUP}"),
                label: None,
            }],
        },
        accept_immediately: false,
        fallback: Some(RouteQueueFallbackConfig {
            failure_code: Some(486),
            failure_reason: Some("All agents busy".to_string()),
            redirect: None,
        }),
        ..Default::default()
    };
    config.queues.insert(QUEUE_NAME.to_string(), queue_config);

    config.routes = Some(vec![RouteRule {
        name: "route_to_rotation_queue".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(QUEUE_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some(QUEUE_NAME.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    config
}

struct Harness {
    server: E2eTestServer,
    _sg_rx: tokio::sync::mpsc::UnboundedReceiver<SkillGroupEvent>,
}

/// Server + CC skill group + adapter drained into the shared RWI gateway
/// whose webhook handler forwards every event to `capture`.
async fn start_harness(capture: &WebhookCapture) -> Result<Harness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("No Answer Rotation Q".to_string()),
            skills_required: vec!["support".to_string()],
            overflow_groups: vec![],
            sla_target_secs: 30,
            max_wait_secs: 90,
            metadata: None,
        },
    )
    .await
    .unwrap();

    let cc_registry = Arc::new(CcAgentRegistry::with_db(db.clone()));
    // alice registers FIRST and goes Idle first — longest-idle ordering
    // makes her the round-1 candidate (she never answers); bob is round 2
    // (he answers).
    for agent_id in ["alice", "bob"] {
        cc_registry
            .register(agent_id.to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status(agent_id, rustpbx::addons::cc::agent::AgentStatus::Idle)
            .await
            .unwrap();
        if agent_id == "alice" {
            sleep(Duration::from_millis(1200)).await;
        }
    }

    let (sg_tx, sg_rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
    let adapter = Arc::new(
        CcAgentRegistryAdapter::new(
            cc_registry,
            Arc::new(AcdEngine::new(AcdConfig {
                enabled: false,
                ..AcdConfig::default()
            })),
            "localhost",
        )
        .with_skill_group_event_tx(sg_tx),
    );

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    // Production drain: SkillGroupEvent → translate → gateway.
    let gw = gateway.clone();
    let mut event_rx = sg_rx;
    let (mirror_tx, mirror_rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let _ = mirror_tx.send(event.clone());
            if let Some(rwi) = translate_skill_group_event(event) {
                gw.read().broadcast_event(&rwi);
            }
        }
    });

    let mut proxy_config = proxy_config();
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;

    let users = vec![
        SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 3,
            username: "caller".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ];

    let server = E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            queue_enricher: None,
            users,
            session_hook: None,
            agent_registry: Some(adapter),
            rwi_gateway: Some(gateway),
            #[cfg(feature = "addon-cc")]
            cc_policy_db: None,
        },
    )
    .await?;

    Ok(Harness {
        server,
        _sg_rx: mirror_rx,
    })
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28100),
        proxy_addr,
    })
}

/// alice's pump: every INVITE rings (180) but is NEVER answered — the queue's
/// ring timeout must CANCEL it and rotate to the next agent.
fn spawn_no_answer_pump(ua: TestUa, invites: Arc<AtomicUsize>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        if let TestUaEvent::IncomingCall(dialog_id, _) = ev {
                            invites.fetch_add(1, Ordering::Relaxed);
                            // A real phone rings (180) but nobody picks up.
                            let _ = ua.ring_call(&dialog_id).await;
                        }
                    }
                }
                Err(_) => break,
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
}

/// bob's pump: answers the FIRST invite (after a short ring).
fn spawn_answer_first_pump(
    ua: TestUa,
    invites: Arc<AtomicUsize>,
    established: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        match ev {
                            TestUaEvent::IncomingCall(dialog_id, offer) => {
                                invites.fetch_add(1, Ordering::Relaxed);
                                let _ = ua.ring_call(&dialog_id).await;
                                sleep(Duration::from_millis(400)).await;
                                let sdp = offer
                                    .as_deref()
                                    .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                    .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                            }
                            TestUaEvent::CallEstablished(_) => {
                                established.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                }
                Err(_) => break,
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
}

async fn wait_webhook_event(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if let Some(ev) = events
                .iter()
                .find(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// 排队 → alice 振铃超时 → 重新入队 → 轮转到 bob → 接听 → 离开队列.
#[tokio::test]
async fn test_no_answer_rotates_to_next_agent() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── Agent legs ───────────────────────────────────────────────────────
    let mut alice = make_ua(proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(300)).await;

    let alice_invites = Arc::new(AtomicUsize::new(0));
    let bob_invites = Arc::new(AtomicUsize::new(0));
    let bob_established = Arc::new(AtomicUsize::new(0));
    let alice_pump = spawn_no_answer_pump(alice.clone(), alice_invites.clone());
    let bob_pump = spawn_answer_first_pump(
        bob.clone(),
        bob_invites.clone(),
        bob_established.clone(),
    );

    // ── Caller → queue (排队) ─────────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(31300),
        false,
    );
    let call = {
        let ua = caller.clone();
        tokio::spawn(async move { ua.make_call(QUEUE_NUMBER, Some(offer)).await })
    };
    let dialog = tokio::time::timeout(Duration::from_secs(15), call)
        .await
        .map_err(|_| anyhow!("caller did not settle within 15s"))?
        .map_err(|e| anyhow!("caller task failed: {e}"))?
        .expect("caller should reach the queue");
    let _ = dialog;

    // ── Round 1: alice is offered and does NOT answer ────────────────────
    let no_answer = wait_webhook_event(
        &capture,
        "queue_agent_no_answer",
        Duration::from_secs(EFFECTIVE_RING_TIMEOUT_SECS + 15),
    )
    .await
    .expect("webhook must receive queue_agent_no_answer after the ring timeout");
    assert_eq!(
        no_answer["event"]["agent_id"].as_str(),
        Some("alice"),
        "round 1 (longest-idle) must go to alice: {no_answer}"
    );
    assert_eq!(
        no_answer["event"]["attempt"].as_u64(),
        Some(1),
        "first ring round must report attempt=1: {no_answer}"
    );

    // ── Per-agent re-entry: queue_joined {attempt:2} ─────────────────────
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);    let rejoined = loop {
        {
            let events = capture.received.lock().unwrap();
            let hit = events.iter().find(|v| {
                v["event_type"].as_str() == Some("queue_joined")
                    && v["event"]["attempt"].as_u64() == Some(2)
            });
            if let Some(ev) = hit {
                break Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break None;
        }
        sleep(Duration::from_millis(100)).await;
    };
    assert!(
        rejoined.is_some(),
        "queue_joined {{attempt:2}} (per-agent re-entry) missing: webhook never saw the requeue"
    );

    // ── Round 2: ROTATION — bob is offered, alice is NOT re-rung ─────────
    wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(20))
        .await
        .expect("bob must answer the rotated offer → queue_agent_connected");

    let left = wait_webhook_event(&capture, "queue_left", Duration::from_secs(15))
        .await
        .expect("webhook must receive queue_left at the terminal transition");
    assert_eq!(
        left["event"]["reason"].as_str(),
        Some("connected"),
        "queue_left must be reason=connected, got: {left}"
    );

    // ── The rotation invariants ──────────────────────────────────────────
    assert_eq!(
        alice_invites.load(Ordering::Relaxed),
        1,
        "alice must be dialed EXACTLY ONCE — a no-answered agent is excluded from the next offer"
    );
    assert_eq!(
        bob_invites.load(Ordering::Relaxed),
        1,
        "bob must be dialed exactly once (the rotated offer)"
    );
    assert_eq!(
        bob_established.load(Ordering::Relaxed),
        1,
        "bob must be established with the caller"
    );

    // ── End the call ─────────────────────────────────────────────────────
    caller.hangup(&dialog).await?;
    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_hangup after the caller hangs up");

    // ── Full Option-A timeline contract ─────────────────────────────────
    let any = capture.received.lock().unwrap().first().cloned();
    let call_id = any
        .as_ref()
        .and_then(|v| v["call_id"].as_str())
        .expect("envelope carries call_id")
        .to_string();

    // queue* ↔ skill_group* never diverge (downstream migration contract).
    RwiTimeline::from_capture(&capture, &call_id).assert_skill_group_parity();
    // Closed set: nothing unexpected leaks into the stream.
    RwiTimeline::from_capture(&capture, &call_id).assert_only_expected(ALLOWED_EVENT_TYPES);

    // Exact sequence for THIS scenario (per-call envelopes, arrival order).
    {
        let events = capture.received.lock().unwrap();
        let evs: Vec<serde_json::Value> = events
            .iter()
            .filter(|v| v["call_id"].as_str() == Some(call_id.as_str()))
            .cloned()
            .collect();
        let types: Vec<&str> = evs
            .iter()
            .filter_map(|v| v["event_type"].as_str())
            .collect();

        // Offered agents, in order: [alice, bob] — exactly one offer each.
        let offered_agents: Vec<&str> = evs
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("queue_agent_offered"))
            .filter_map(|v| v["event"]["agent_id"].as_str())
            .collect();
        assert_eq!(
            offered_agents,
            vec!["alice", "bob"],
            "offers must rotate alice → bob with no repeats: {types:?}"
        );

        // Exactly one no-answer (alice) and one connect (bob).
        let no_answers: Vec<&str> = evs
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("queue_agent_no_answer"))
            .filter_map(|v| v["event"]["agent_id"].as_str())
            .collect();
        assert_eq!(no_answers, vec!["alice"], "one no-answer (alice): {types:?}");
        let connected: Vec<&str> = evs
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("queue_agent_connected"))
            .filter_map(|v| v["event"]["agent_id"].as_str())
            .collect();
        assert_eq!(connected, vec!["bob"], "one connect (bob): {types:?}");

        // Option-A: queue_left is terminal and only reason=connected.
        let left_count = types
            .iter()
            .filter(|t| **t == "queue_left")
            .count();
        assert_eq!(left_count, 1, "exactly one queue_left (terminal): {types:?}");

        // Ordering: no_answer(alice) < queue_joined{attempt:2} < offered(bob).
        let idx = |pred: &dyn Fn(&serde_json::Value) -> bool| {
            evs.iter().position(|v| pred(v))
        };
        let no_answer_idx = idx(&|v| {
            v["event_type"].as_str() == Some("queue_agent_no_answer")
        })
        .expect("no_answer present");
        let joined2_idx = idx(&|v| {
            v["event_type"].as_str() == Some("queue_joined")
                && v["event"]["attempt"].as_u64() == Some(2)
        })
        .expect("queue_joined{attempt:2} present");
        let offered_bob_idx = idx(&|v| {
            v["event_type"].as_str() == Some("queue_agent_offered")
                && v["event"]["agent_id"].as_str() == Some("bob")
        })
        .expect("bob offered present");
        assert!(
            no_answer_idx < joined2_idx && joined2_idx < offered_bob_idx,
            "requeue must sit between alice's failure and bob's offer: {types:?}"
        );

        println!("── RWI timeline (call {call_id}) ──");
        for (i, ev) in evs.iter().enumerate() {
            let t = types[i];
            if t.starts_with("queue_") || t.starts_with("skill_group_") {
                println!(
                    "  {:2} {:36} agent={:?} attempt={:?} reason={:?}",
                    i,
                    t,
                    ev["event"]["agent_id"].as_str(),
                    ev["event"]["attempt"].as_u64(),
                    ev["event"]["reason"].as_str(),
                );
            }
        }
        println!("── end timeline ──");
    }

    alice_pump.abort();
    bob_pump.abort();
    let _ = alice.stop();
    let _ = bob.stop();
    let _ = caller.stop();
    harness.server.stop();
    Ok(())
}
