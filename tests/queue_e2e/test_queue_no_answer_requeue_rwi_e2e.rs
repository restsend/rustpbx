//! No-answer retry full RWI contract e2e (Option-A dequeue semantics):
//!
//! 排队 → 分配 → 振铃 → **无人应答再排队** → 再分配 → 再振铃 → 接听 → 离开队列.
//!
//! One agent (bob) is Idle from the start; the caller dials the queue
//! directly. bob's phone rings (180) but he does NOT pick up; the per-agent
//! ring timeout fires, the ringing leg is CANCELled and the call is
//! re-queued — the second ring is answered.
//!
//! RWI webhook contract under test:
//!
//! 1. `queue_joined` — caller queued
//! 2. `skill_group_candidates_found` + `skill_group_agent_assigned` — round 1
//! 3. `queue_agent_offered` — ring starts; **no `queue_left` may appear**
//! 4. `queue_agent_no_answer {attempt:1}` — ring timeout
//! 5. re-queue: `skill_group_candidates_found` + `skill_group_agent_assigned`
//!    again (round 2) — still no `queue_left`
//! 6. `queue_agent_offered` (round 2)
//! 7. `queue_agent_connected` — bob answers
//! 8. `queue_left {reason:"connected"}` — the ONLY `queue_left`, strictly
//!    AFTER `queue_agent_connected` (dequeue is terminal, never at
//!    assignment/ring time)
//! 9. `call_hangup`

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

const QUEUE_NUMBER: &str = "9300";
const QUEUE_NAME: &str = "noanswer_q";
const SKILL_GROUP: &str = "sg_no_answer";
/// Per-agent ring timeout (maps to the queue plan `ring_timeout`).
const RING_TIMEOUT_SECS: u64 = 2;

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
    "skill_group_agent_connected",
    "skill_group_call_left",
];

/// Closed set for [`RwiTimeline::assert_only_expected`] — every event type
/// a no-answer-retry call may legitimately produce. Anything else leaking
/// into the stream fails the test (subscription drift / unknown events).
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
        name: "route_to_no_answer_queue".to_string(),
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
            display_name: Some("No Answer Q".to_string()),
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
    cc_registry
        .register("bob".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    // Agent starts Idle: the caller is assigned and dialed immediately.
    cc_registry
        .update_status("bob", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();

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
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
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

/// Agent UA pump: INVITE #1 rings (180) but is never answered — the queue's
/// ring timeout must CANCEL it and re-dial. INVITE #2+ rings and answers.
fn spawn_agent_pump(
    mut ua: TestUa,
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
                                let n = invites.fetch_add(1, Ordering::Relaxed) + 1;
                                // A real phone rings (180) on every invite.
                                let _ = ua.ring_call(&dialog_id).await;
                                if n >= 2 {
                                    // Second round: pick up.
                                    sleep(Duration::from_millis(400)).await;
                                    let sdp = offer
                                        .as_deref()
                                        .map(|o| {
                                            create_test_sdp_answer(o, "127.0.0.1", 0)
                                        })
                                        .unwrap_or_else(|| {
                                            create_test_sdp("127.0.0.1", 0, false)
                                        });
                                    let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                                }
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

/// 排队 → 分配 → 振铃 → 无应答再排队 → 再振铃 → 接听 → 离开队列.
#[tokio::test]
async fn test_no_answer_requeue_rwi_event_contract() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── Agent leg ────────────────────────────────────────────────────────
    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(300)).await;

    let invites = Arc::new(AtomicUsize::new(0));
    let established = Arc::new(AtomicUsize::new(0));
    let bob_pump = spawn_agent_pump(bob.clone(), invites.clone(), established.clone());

    // ── Caller → queue (排队) ─────────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(31200),
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

    // ── Round 1: assigned → offered → NO answer ──────────────────────────
    let no_answer = wait_webhook_event(&capture, "queue_agent_no_answer", Duration::from_secs(20))
        .await
        .expect("webhook must receive queue_agent_no_answer after the ring timeout");
    assert_eq!(
        no_answer["event"]["agent_id"].as_str(),
        Some("bob"),
        "no_answer: {no_answer}"
    );
    assert_eq!(
        no_answer["event"]["attempt"].as_u64(),
        Some(1),
        "first ring round must report attempt=1: {no_answer}"
    );

    // ── Round 2: re-assigned → offered again → answered ──────────────────
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        {
            let events = capture.received.lock().unwrap();
            let offered_cnt = events
                .iter()
                .filter(|v| v["event_type"].as_str() == Some("queue_agent_offered"))
                .count();
            if offered_cnt >= 2 {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("second queue_agent_offered never arrived (re-dial after no-answer failed)");
        }
        sleep(Duration::from_millis(100)).await;
    }

    wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(15))
        .await
        .expect("bob answers the second ring → queue_agent_connected");

    let left = wait_webhook_event(&capture, "queue_left", Duration::from_secs(15))
        .await
        .expect("webhook must receive queue_left at the terminal transition");
    assert_eq!(
        left["event"]["reason"].as_str(),
        Some("connected"),
        "queue_left must be reason=connected, got: {left}"
    );

    assert_eq!(
        invites.load(Ordering::Relaxed),
        2,
        "bob must be dialed exactly twice (no-answer retry)"
    );
    let established_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while established.load(Ordering::Relaxed) == 0
        && tokio::time::Instant::now() < established_deadline
    {
        sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        established.load(Ordering::Relaxed),
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

    // Attribution + ordering + the dequeue invariants, shared with the
    // full-chain e2e.
    RwiTimeline::from_capture(&capture, &call_id).assert_queue_agent_contract("bob");
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

        let pos_all = |name: &str| -> Vec<usize> {
            types
                .iter()
                .enumerate()
                .filter(|(_, t)| **t == name)
                .map(|(i, _)| i)
                .collect()
        };

        let joined = pos_all("queue_joined");
        let sg_joined = pos_all("skill_group_call_joined");
        let assigned = pos_all("skill_group_agent_assigned");
        let offered = pos_all("queue_agent_offered");
        let no_answers = pos_all("queue_agent_no_answer");
        let sg_no_answers = pos_all("skill_group_agent_no_answer");
        let connected = pos_all("queue_agent_connected");
        let sg_connected = pos_all("skill_group_agent_connected");
        let lefts = pos_all("queue_left");
        let sg_lefts = pos_all("skill_group_call_left");

        assert!(!joined.is_empty(), "queue_joined missing: {types:?}");
        assert_eq!(offered.len(), 2, "exactly two ringing rounds: {types:?}");
        assert_eq!(no_answers.len(), 1, "exactly one no-answer: {types:?}");
        assert_eq!(connected.len(), 1, "exactly one connect: {types:?}");
        assert_eq!(lefts.len(), 1, "exactly one queue_left: {types:?}");

        // ── skill_group_* data-analysis events (the downstream contract) ──
        // Join: bob is Idle from the start → the call joins AND is assigned
        // in the same round → reason "immediate" (this scenario is exactly
        // the case `skill_group_call_queued` does NOT cover).
        assert_eq!(sg_joined.len(), 1, "one skill_group_call_joined: {types:?}");
        assert_eq!(
            evs[sg_joined[0]]["event"]["reason"].as_str(),
            Some("immediate"),
            "idle agent → immediate join+assign: {:#}",
            evs[sg_joined[0]]
        );
        assert!(
            evs[sg_joined[0]]["event"]["queue_depth"].as_u64().is_some(),
            "skill_group_call_joined must carry queue_depth: {:#}",
            evs[sg_joined[0]]
        );

        // No-answer round mirrors queue_agent_no_answer (attempt 1).
        assert_eq!(
            sg_no_answers.len(),
            1,
            "one skill_group_agent_no_answer: {types:?}"
        );
        assert_eq!(
            evs[sg_no_answers[0]]["event"]["attempt"].as_u64(),
            Some(1),
            "no-answer round must carry attempt=1: {:#}",
            evs[sg_no_answers[0]]
        );
        assert_eq!(
            evs[sg_no_answers[0]]["event"]["agent_id"].as_str(),
            Some("bob"),
            "no-answer must attribute bob: {:#}",
            evs[sg_no_answers[0]]
        );

        // Connected on round 2: attempt aligns with the winning assignment.
        assert_eq!(
            sg_connected.len(),
            1,
            "one skill_group_agent_connected: {types:?}"
        );
        assert_eq!(
            evs[sg_connected[0]]["event"]["attempt"].as_u64(),
            Some(2),
            "connect must be round 2: {:#}",
            evs[sg_connected[0]]
        );
        assert!(
            evs[sg_connected[0]]["event"]["wait_secs"].as_u64().is_some(),
            "skill_group_agent_connected must carry wait_secs: {:#}",
            evs[sg_connected[0]]
        );

        // Terminal: skill_group_call_left{connected} with the group history.
        assert_eq!(
            sg_lefts.len(),
            1,
            "one skill_group_call_left: {types:?}"
        );
        assert_eq!(
            evs[sg_lefts[0]]["event"]["reason"].as_str(),
            Some("connected"),
            "terminal reason: {:#}",
            evs[sg_lefts[0]]
        );
        assert_eq!(
            evs[sg_lefts[0]]["event"]["skill_groups"],
            serde_json::json!([SKILL_GROUP]),
            "terminal carries the full group history: {:#}",
            evs[sg_lefts[0]]
        );

        // 排队 precedes every assignment.
        assert!(
            assigned.iter().all(|&a| a > joined[0]),
            "skill_group_agent_assigned must follow queue_joined: {types:?}"
        );
        // Round boundaries: offered(1) < no_answer < offered(2) < connected.
        assert!(
            offered[0] < no_answers[0]
                && no_answers[0] < offered[1]
                && offered[1] < connected[0],
            "round ordering violated: {types:?}"
        );
        // THE Option-A invariant: no queue_left anywhere before the connect.
        assert!(
            lefts.iter().all(|&l| l > connected[0]),
            "queue_left must be strictly after queue_agent_connected \
             (dequeue is terminal, never at assignment/ring time): {types:?}"
        );
        // Every ringing round is announced by an assignment first.
        assert!(
            assigned.len() >= 2,
            "each round must be preceded by skill_group_agent_assigned: {types:?}"
        );
        // Assignment round markers: attempt increments per re-assignment
        // (1-based) so consumers can tell which round an event belongs to.
        let attempts: Vec<u64> = evs
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("skill_group_agent_assigned"))
            .map(|v| v["event"]["attempt"].as_u64().unwrap_or(0))
            .collect();
        assert_eq!(
            attempts,
            vec![1, 2],
            "skill_group_agent_assigned.attempt must count assignment rounds 1..N: {types:?}"
        );

        // Real-data dump for the run log (visible with --nocapture).
        println!("── RWI timeline (call {call_id}) ──");
        for (i, ev) in evs.iter().enumerate() {
            let t = types[i];
            if [
                "queue_joined",
                "skill_group_call_joined",
                "skill_group_call_queued",
                "skill_group_candidates_found",
                "skill_group_agent_assigned",
                "queue_agent_offered",
                "queue_agent_no_answer",
                "skill_group_agent_no_answer",
                "queue_agent_connected",
                "skill_group_agent_connected",
                "queue_left",
                "skill_group_call_left",
            ]
            .contains(&t)
            {
                println!(
                    "  {:2} {:36} agent={:?} attempt={:?} reason={:?} leg={:?} depth={:?}",
                    i,
                    t,
                    ev["event"]["agent_id"].as_str(),
                    ev["event"]["attempt"].as_u64(),
                    ev["event"]["reason"].as_str(),
                    ev["event"]["leg_id"].as_str(),
                    ev["event"]["queue_depth"].as_u64(),
                );
            }
        }
        println!("── end timeline ──");
    }

    bob_pump.abort();
    Ok(())
}
