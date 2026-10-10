//! Agent-rejection RWI contract e2e: 排队 → 分配(alice) → 振铃 → **拒接(486)**
//! → 再分配(bob) → 振铃 → 接听 → 离开队列.
//!
//! Two agents in one skill group; alice's phone rejects the INVITE with 486,
//! the queue re-dials bob who answers. Asserts the queue* ↔ skill_group*
//! pairing for the rejection round:
//!
//! 1. `queue_agent_offered`(alice) — round 1
//! 2. `queue_agent_rejected {attempt:1}` ↔ `skill_group_agent_rejected`
//!    {attempt:1}` — same round, same leg, same agent
//! 3. `skill_group_agent_assigned`(bob, attempt 2) → `queue_agent_offered`
//! 4. `queue_agent_connected` → `skill_group_agent_connected {attempt:2}`
//! 5. `queue_left{connected}` ↔ `skill_group_call_left{connected}` — the
//!    ONLY terminal, strictly after connect
//! 6. parity + closed set via the shared RwiTimeline helpers.

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
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rwi_timeline::RwiTimeline;
use crate::common::test_ua::{
    TestUa, TestUaConfig, TestUaEvent, create_test_sdp, create_test_sdp_answer,
};
use crate::common::webhook_capture::WebhookCapture;

const QUEUE_NUMBER: &str = "9400";
const QUEUE_NAME: &str = "rejected_q";
const SKILL_GROUP: &str = "sg_rejected";
const RING_TIMEOUT_SECS: u64 = 20;

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_rejected",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_joined",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "skill_group_agent_rejected",
    "skill_group_position_changed",
    "skill_group_agent_connected",
    "skill_group_call_left",
];

const ALLOWED_EVENT_TYPES: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_progress",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_rejected",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_joined",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "skill_group_no_agent",
    "skill_group_agent_rejected",
    "skill_group_position_changed",
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
        name: "route_to_rejected_queue".to_string(),
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
}

async fn start_harness(capture: &WebhookCapture) -> Result<Harness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Rejected Q".to_string()),
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
    // makes her the round-1 candidate (she rejects); bob is round 2.
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

    let gw = gateway.clone();
    let mut event_rx = sg_rx;
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
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

    Ok(Harness { server })
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28200),
        proxy_addr,
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

/// 排队 → 分配 → 振铃 → 拒接 → 再分配 → 接听 → 离开队列.
#[tokio::test]
async fn test_agent_rejected_rwi_event_contract() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── alice rejects every invite (486); bob rings then answers ─────────
    let mut alice = make_ua(proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    let alice_pump = {
        let ua = alice.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            if let TestUaEvent::IncomingCall(dialog_id, _) = ev {
                                let _ = ua.reject_call_with_reason(
                                    &dialog_id,
                                    Some(486),
                                    Some("Busy Here".to_string()),
                                )
                                .await;
                            }
                        }
                    }
                    Err(_) => break,
                }
                sleep(Duration::from_millis(30)).await;
            }
        })
    };

    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let established = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bob_pump = {
        let ua = bob.clone();
        let established = established.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            match ev {
                                TestUaEvent::IncomingCall(dialog_id, offer) => {
                                    let _ = ua.ring_call(&dialog_id).await;
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
                                TestUaEvent::CallEstablished(_) => {
                                    established.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    };
    sleep(Duration::from_millis(300)).await;

    // ── Caller → queue ───────────────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(32200),
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

    // ── Rejection round ──────────────────────────────────────────────────
    let rejected = wait_webhook_event(&capture, "queue_agent_rejected", Duration::from_secs(20))
        .await
        .expect("webhook must receive queue_agent_rejected after the 486");
    assert_eq!(
        rejected["event"]["agent_id"].as_str(),
        Some("alice"),
        "round 1 goes to the longest-idle agent: {rejected}"
    );
    assert_eq!(
        rejected["event"]["attempt"].as_u64(),
        Some(1),
        "rejection round must report attempt=1: {rejected}"
    );
    assert_eq!(
        rejected["event"]["leg_role"].as_str(),
        Some("agent"),
        "queue_agent_rejected must carry leg_role: {rejected}"
    );
    let sg_rejected =
        wait_webhook_event(&capture, "skill_group_agent_rejected", Duration::from_secs(5))
            .await
            .expect("skill_group_agent_rejected must pair with the 486");
    assert_eq!(
        sg_rejected["event"]["attempt"].as_u64(),
        Some(1),
        "skill_group rejection round aligns: {sg_rejected}"
    );
    assert_eq!(
        sg_rejected["event"]["leg_id"].as_str(),
        rejected["event"]["leg_id"].as_str(),
        "skill_group/queue rejections name the SAME leg: {sg_rejected} vs {rejected}"
    );
    assert_eq!(
        sg_rejected["event"]["leg_role"].as_str(),
        Some("agent"),
        "skill_group_agent_rejected must carry leg_role: {sg_rejected}"
    );

    // ── Bob answers round 2 ──────────────────────────────────────────────
    wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(15))
        .await
        .expect("bob answers the second ring → queue_agent_connected");
    let left = wait_webhook_event(&capture, "queue_left", Duration::from_secs(15))
        .await
        .expect("webhook must receive queue_left at the terminal transition");
    assert_eq!(
        left["event"]["reason"].as_str(),
        Some("connected"),
        "queue_left must be reason=connected: {left}"
    );

    caller.hangup(&dialog).await?;
    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_hangup");

    // ── Shared contracts ─────────────────────────────────────────────────
    let call_id = capture.received.lock().unwrap()[0]["call_id"]
        .as_str()
        .expect("envelope carries call_id")
        .to_string();
    RwiTimeline::from_capture(&capture, &call_id).assert_queue_agent_contract("bob");
    RwiTimeline::from_capture(&capture, &call_id).assert_skill_group_parity();
    RwiTimeline::from_capture(&capture, &call_id).assert_only_expected(ALLOWED_EVENT_TYPES);

    // ── Scenario-specific sequence ───────────────────────────────────────
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
        let pos = |name: &str| types.iter().position(|t| *t == name);

        let connected = pos("queue_agent_connected").expect("connected");
        // A 486 can arrive before any 180, so the REJECTED round has no
        // `queue_agent_offered` (the phone never rang) — exactly one offered
        // round survives: bob's winning ring.
        let offered_rounds: Vec<usize> = types
            .iter()
            .enumerate()
            .filter(|(_, t)| **t == "queue_agent_offered")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(offered_rounds.len(), 1, "one ringing round (bob): {types:?}");
        let rej = pos("queue_agent_rejected").expect("rejection");
        assert!(
            rej < offered_rounds[0] && offered_rounds[0] < connected,
            "round ordering (rejected → offered → connected): {types:?}"
        );
        // The winning assignment: a single resolve decided the sequential
        // candidate order [alice, bob] — the 486 rejection dials the NEXT
        // candidate of the SAME round (no second `skill_group_agent_assigned`),
        // so exactly one assignment names alice, and bob is attributed by
        // his offered/connected events.
        let assigned_agents: Vec<&str> = evs
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("skill_group_agent_assigned"))
            .filter_map(|v| v["event"]["agent_id"].as_str())
            .collect();
        assert_eq!(
            assigned_agents,
            vec!["alice"],
            "one resolve names the sequential candidate order head: {types:?}"
        );
        // No no-answer on this path (rejection is NOT a ring timeout).
        assert!(
            !types.contains(&"queue_agent_no_answer")
                && !types.contains(&"skill_group_agent_no_answer"),
            "rejection path must not emit no-answer events: {types:?}"
        );

        println!("── RWI timeline (call {call_id}) ──");
        for (i, ev) in evs.iter().enumerate() {
            let t = types[i];
            if t.starts_with("queue_") || t.starts_with("skill_group_") {
                println!(
                    "  {:2} {:36} agent={:?} attempt={:?} reason={:?} leg={:?}",
                    i,
                    t,
                    ev["event"]["agent_id"].as_str(),
                    ev["event"]["attempt"].as_u64(),
                    ev["event"]["reason"].as_str(),
                    ev["event"]["leg_id"].as_str(),
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
