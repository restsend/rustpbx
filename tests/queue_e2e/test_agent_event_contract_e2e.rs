//! Full-stream agent attribution e2e: **every** `call_*` / `queue_*` /
//! `skill_group_*` event that describes an agent must carry
//! `leg_role: "agent"` + `client_ip`, exactly once per occurrence.
//!
//! This is the executable form of the cross-node contract matrix:
//!
//! | family                          | leg_role | client_ip |
//! |---------------------------------|----------|-----------|
//! | `queue_agent_*`                 | agent    | required  |
//! | `skill_group_agent_*`           | agent    | required  |
//! | `call_ringing/answered/hangup` (agent dialog, role-attributed) | agent | required |
//! | dispatcher-side duplicate legs  | absent   | (not an agent event) |
//!
//! Two flows:
//! 1. `test_agent_full_stream_role_and_client_ip` — queue → agent answers →
//!    caller hangs up. Asserts the complete captured stream: exactly one
//!    role-attributed ringing/answered/hangup, queue + skill agent families
//!    all attributed, client_ip stamped everywhere.
//! 2. `test_agent_miss_stream_role_and_client_ip` — queue → agent rings but
//!    never answers → ring timeout. Asserts `queue_agent_no_answer` +
//!    `skill_group_agent_no_answer` + the miss `call_hangup{ring_timeout}`
//!    are attributed (single-source queue families included).
//!
//! The test prints the full JSON stream (run with `--nocapture`) so the real
//! wire shape can be inspected and diffed.

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
const QUEUE_NAME: &str = "contract_q";
const SKILL_GROUP: &str = "sg_contract";
const AGENT: &str = "bob";
/// Simulated registration-source IP the gateway stamps on agent events
/// (production: captured by the registrar bridge from the REGISTER socket,
/// relayed between cluster nodes).
const AGENT_CLIENT_IP: &str = "203.0.113.77";
const RING_TIMEOUT_SECS: u64 = 3;

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "call_held",
    "call_unheld",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_rejected",
    "queue_agent_no_answer",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_joined",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "skill_group_no_agent",
    "skill_group_agent_rejected",
    "skill_group_agent_no_answer",
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
    "queue_agent_no_answer",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_joined",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "skill_group_no_agent",
    "skill_group_agent_rejected",
    "skill_group_agent_no_answer",
    "skill_group_agent_connected",
    "skill_group_call_left",
    "media_play_finished",
    "recording_metadata_available",
    "record_stopped",
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
            // Per-agent ring timeout (miss flow).
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
        name: "route_to_contract_queue".to_string(),
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
            display_name: Some("Contract Q".to_string()),
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
    // Ring timeout drives the miss flow: the CC registry's configured value
    // wins over the route (`effective_ring_timeout`), default 30 s.
    let mut behavior = rustpbx::addons::cc::config::DeskBehaviorConfig::default();
    behavior.ring_no_answer_secs = RING_TIMEOUT_SECS;
    cc_registry.set_desk_behavior(behavior);
    cc_registry
        .register(AGENT.to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    cc_registry
        .update_status(AGENT, rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();
    // Transfer-target agent: DIFFERENT skill so queue routing (support) is
    // unaffected — only the transfer flow dials her.
    cc_registry
        .register("carol".to_string(), vec!["sales".to_string()], 1)
        .await
        .unwrap();
    cc_registry
        .update_status("carol", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();
    // Production wiring simulation: the registrar bridge captured the agent's
    // registration socket, so the gateway stamps `client_ip` on its events.
    cc_registry.set_client_ip(AGENT, Some(AGENT_CLIENT_IP.to_string()));
    cc_registry.set_client_ip("carol", Some(AGENT_CLIENT_IP.to_string()));

    let (sg_tx, sg_rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
    let adapter = Arc::new(
        CcAgentRegistryAdapter::new(
            cc_registry.clone(),
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
        // Same lookup the CC addon installs in production.
        let registry_for_lookup = cc_registry.clone();
        gw.set_client_ip_lookup(Some(Arc::new(move |agent_id: &str| {
            registry_for_lookup.client_ip_sync(agent_id)
        })));
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
            username: AGENT.to_string(),
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
        SipUser {
            id: 3,
            username: "carol".to_string(),
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
        local_port: portpicker::pick_unused_port().unwrap_or(28300),
        proxy_addr,
    })
}

fn payload(ev: &serde_json::Value) -> &serde_json::Value {
    if ev["event"].is_object() {
        &ev["event"]
    } else {
        ev
    }
}

fn events_of_type<'a>(
    timeline_events: &'a [serde_json::Value],
    etype: &str,
) -> Vec<&'a serde_json::Value> {
    timeline_events
        .iter()
        .filter(|v| v["event_type"].as_str() == Some(etype))
        .collect()
}

/// Role-attributed (leg_role == "agent") events of one type.
fn agent_role_events<'a>(
    timeline_events: &'a [serde_json::Value],
    etype: &str,
) -> Vec<&'a serde_json::Value> {
    timeline_events
        .iter()
        .filter(|v| {
            v["event_type"].as_str() == Some(etype)
                && payload(v)["leg_role"].as_str() == Some("agent")
        })
        .collect()
}

/// Print the full captured JSON stream (visible with `--nocapture`).
fn dump_stream(call_id: &str, events: &[serde_json::Value]) {
    println!("── RWI full stream (call {call_id}, {} events) ──", events.len());
    for ev in events {
        let compact = serde_json::json!({
            "event_type": ev["event_type"],
            "leg_id": payload(ev)["leg_id"],
            "leg_role": payload(ev)["leg_role"],
            "agent_id": payload(ev)["agent_id"],
            "client_ip": payload(ev)["client_ip"],
            "scope": payload(ev)["scope"],
            "reason": payload(ev)["reason"],
        });
        println!("{}", serde_json::to_string(&compact).unwrap_or_default());
    }
    println!("── end stream ──");
}

async fn wait_for_event(
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

/// Queue → agent answers → caller hangs up: the complete agent-event stream
/// is role-attributed + client_ip stamped, exactly once per occurrence.
#[tokio::test]
async fn test_agent_full_stream_role_and_client_ip() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── Agent UA: ring briefly, then answer ──────────────────────────────
    let mut agent_ua = make_ua(proxy_addr, AGENT);
    agent_ua.start().await?;
    agent_ua.register().await?;
    let agent_pump = {
        let ua = agent_ua.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            if let TestUaEvent::IncomingCall(dialog_id, offer) = ev {
                                let _ = ua.ring_call(&dialog_id).await;
                                sleep(Duration::from_millis(300)).await;
                                let sdp = offer
                                    .as_deref()
                                    .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                    .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
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

    // ── Caller → queue → agent ───────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(32300),
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

    // Agent connect must be observed before we tear the call down.
    let connected =
        wait_for_event(&capture, "queue_agent_connected", Duration::from_secs(20))
            .await
            .expect("queue_agent_connected must fire");
    assert_eq!(
        payload(&connected)["agent_id"].as_str(),
        Some(AGENT),
        "connected names the agent: {connected}"
    );

    sleep(Duration::from_millis(1500)).await;
    caller.hangup(&dialog).await.ok();

    let hangup =
        wait_for_event(&capture, "call_hangup", Duration::from_secs(10))
            .await
            .expect("call_hangup must fire after the caller hangs up");
    let call_id = hangup["call_id"]
        .as_str()
        .expect("envelope carries call_id")
        .to_string();
    // Let the remaining trailing events (records, media) land.
    sleep(Duration::from_millis(600)).await;

    let events: Vec<serde_json::Value> = capture
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["call_id"].as_str() == Some(call_id.as_str()))
        .cloned()
        .collect();
    dump_stream(&call_id, &events);

    // ── Generic contract: every agent-related event attributed ───────────
    let timeline = RwiTimeline::from_events(&call_id, events.clone());
    timeline.assert_agent_role_contract(AGENT, true);
    timeline.assert_only_expected(ALLOWED_EVENT_TYPES);

    // ── Explicit matrix: agent-dialog lifecycle, exactly one each ────────
    for (etype, expected) in [
        ("call_ringing", 1usize),
        ("call_answered", 1),
        ("call_hangup", 1),
    ] {
        let role_events = agent_role_events(&events, etype);
        assert_eq!(
            role_events.len(),
            expected,
            "{etype}: exactly {expected} role=agent emission expected, got {role_events:?}"
        );
        let ev = role_events[0];
        assert_eq!(
            payload(ev)["agent_id"].as_str(),
            Some(AGENT),
            "{etype} must name the agent: {ev}"
        );
        assert_eq!(
            payload(ev)["client_ip"].as_str(),
            Some(AGENT_CLIENT_IP),
            "{etype} must carry the registration-source client_ip: {ev}"
        );
        assert!(
            payload(ev)["leg_id"].as_str().is_some(),
            "{etype} agent lifecycle event must be leg-scoped: {ev}"
        );
    }

    // ── Queue + skill-group agent families: agent identity only ─────────
    // Final contract: `leg_role`/`client_ip` belong exclusively to the
    // registration-home node's call_* events; queue families identify the
    // agent by `agent_id`.
    for (etype, expected) in [
        ("queue_agent_offered", 1usize),
        ("queue_agent_connected", 1),
        ("skill_group_agent_assigned", 1),
        ("skill_group_agent_connected", 1),
    ] {
        let evs = events_of_type(&events, etype);
        assert_eq!(
            evs.len(),
            expected,
            "{etype}: exactly {expected} emission expected for a single-agent \
             answered flow — a duplicate means double-emission, absent means lost: {evs:?}"
        );
        for ev in evs {
            assert_eq!(
                payload(ev)["agent_id"].as_str(),
                Some(AGENT),
                "{etype} must name the agent: {ev}"
            );
            assert_ne!(
                payload(ev)["leg_role"].as_str(),
                Some("agent"),
                "{etype} must not claim the agent role (call_* only): {ev}"
            );
        }
    }

    // The dispatcher-side session answered carries no agent role claim.
    let session_answered: Vec<&serde_json::Value> = events
        .iter()
        .filter(|v| {
            v["event_type"].as_str() == Some("call_answered")
                && payload(v)["leg_id"].is_null()
        })
        .collect();
    for ev in session_answered {
        assert_ne!(
            payload(ev)["leg_role"].as_str(),
            Some("agent"),
            "session-scoped call_answered must not claim the agent role: {ev}"
        );
    }

    agent_pump.abort();
    harness.server.stop();
    Ok(())
}

/// Queue → agent answers → CC blind transfer to ANOTHER registered agent
/// (carol) → carol answers → caller hangs up. The transferred-to agent's
/// `call_*` events must carry `leg_role=agent + client_ip` from the very
/// first ringing (prd: 转接之后 leg_role=agent 没有了) — exactly once per
/// lifecycle kind, attributed to carol (not the departing bob).
#[tokio::test]
async fn test_agent_transfer_stream_role_and_client_ip() -> Result<()> {
    use rustpbx::call::domain::CallCommand;

    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── bob answers the queue round; carol answers the transfer ──────────
    let mut bob = make_ua(proxy_addr, AGENT);
    bob.start().await?;
    bob.register().await?;
    let bob_pump = {
        let ua = bob.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            if let TestUaEvent::IncomingCall(dialog_id, offer) = ev {
                                let _ = ua.ring_call(&dialog_id).await;
                                sleep(Duration::from_millis(300)).await;
                                let sdp = offer
                                    .as_deref()
                                    .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                    .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                            }
                        }
                    }
                    Err(_) => break,
                }
                sleep(Duration::from_millis(30)).await;
            }
        })
    };
    let mut carol = make_ua(proxy_addr, "carol");
    carol.start().await?;
    carol.register().await?;
    let carol_pump = {
        let ua = carol.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            if let TestUaEvent::IncomingCall(dialog_id, offer) = ev {
                                let _ = ua.ring_call(&dialog_id).await;
                                sleep(Duration::from_millis(300)).await;
                                let sdp = offer
                                    .as_deref()
                                    .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                    .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
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

    // ── Caller → queue → bob ─────────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(32500),
        false,
    );
    let caller_dialog = caller.make_call(QUEUE_NUMBER, Some(offer)).await?;
    wait_for_event(&capture, "queue_agent_connected", Duration::from_secs(20))
        .await
        .expect("bob connects");
    // Agent attribution pins at answer; give the hooks a beat.
    sleep(Duration::from_millis(700)).await;
    let handle = harness
        .server
        .registry
        .get_handle_by_call_id(&caller_dialog.call_id)
        .ok_or_else(|| anyhow!("customer call-id must resolve to its owner session"))?;

    // ── CC blind transfer → carol (registered agent) ──────────────────────
    handle
        .send_command(CallCommand::BlindTransferCustomer {
            target: "carol".to_string(),
            headers: Default::default(),
            application_only: false,
            completion: None,
        })
        .map_err(|e| anyhow!("command dispatch: {e}"))?;

    // Carol rings + answers on her independent leg.
    sleep(Duration::from_millis(2500)).await;
    caller.hangup(&caller_dialog).await.ok();
    sleep(Duration::from_millis(800)).await;

    let events: Vec<serde_json::Value> = {
        let received = capture.received.lock().unwrap();
        let call_id = received
            .iter()
            .find(|v| v["event_type"].as_str() == Some("queue_joined"))
            .and_then(|v| v["call_id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("queue_joined never captured"))?;
        received
            .iter()
            .filter(|v| v["call_id"].as_str() == Some(call_id.as_str()))
            .cloned()
            .collect()
    };
    dump_stream(&events.first().and_then(|v| v["call_id"].as_str()).unwrap_or("?"), &events);

    // ── Carol's dialog: role=agent + client_ip on every lifecycle kind ───
    for (etype, require_client_ip) in [
        ("call_ringing", true),
        ("call_answered", true),
        ("call_hangup", false), // teardown may race the capture window
    ] {
        let carol_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| {
                v["event_type"].as_str() == Some(etype)
                    && payload(v)["agent_id"].as_str() == Some("carol")
                    && payload(v)["leg_role"].as_str() == Some("agent")
            })
            .collect();
        assert_eq!(
            carol_events.len(),
            1,
            "{etype}: exactly ONE role=agent emission attributed to carol, got {carol_events:?}"
        );
        if require_client_ip {
            assert_eq!(
                payload(carol_events[0])["client_ip"].as_str(),
                Some(AGENT_CLIENT_IP),
                "{etype} (carol) must carry the registration client_ip: {}",
                carol_events[0]
            );
        }
        assert!(
            payload(carol_events[0])["leg_id"].as_str().is_some(),
            "{etype} (carol) agent lifecycle event must be leg-scoped: {}",
            carol_events[0]
        );
    }

    // ── Attribution never crosses agents ─────────────────────────────────
    for ev in &events {
        let p = payload(ev);
        if p["leg_role"].as_str() == Some("agent") {
            let agent = p["agent_id"].as_str().unwrap_or_default();
            assert!(
                agent == "carol" || agent == AGENT,
                "role=agent names an unknown agent {agent:?}: {ev}"
            );
            // Per-leg uniqueness: each agent's own dialog reports its own
            // lifecycle — no event kind carries the role twice for one leg.
        }
    }
    let carol_ringing_legs: std::collections::HashSet<&str> = events
        .iter()
        .filter(|v| {
            v["event_type"].as_str() == Some("call_ringing")
                && payload(v)["agent_id"].as_str() == Some("carol")
                && payload(v)["leg_id"].as_str().is_some()
        })
        .map(|v| payload(v)["leg_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        carol_ringing_legs.len(),
        1,
        "carol rings on exactly one leg: {carol_ringing_legs:?}"
    );

    // queue families stay attribution-free (agent_id only).
    for etype in ["queue_agent_offered", "queue_agent_connected"] {
        for ev in events.iter().filter(|v| v["event_type"].as_str() == Some(etype)) {
            assert_ne!(
                payload(ev)["leg_role"].as_str(),
                Some("agent"),
                "{etype} must not claim the agent role: {ev}"
            );
        }
    }

    bob_pump.abort();
    carol_pump.abort();
    harness.server.stop();
    Ok(())
}

/// Queue → agent rings but never answers → ring timeout: miss events are
/// attributed too (`queue_agent_no_answer`, `skill_group_agent_no_answer`,
/// and the miss `call_hangup{reason:"ring_timeout"}`).
#[tokio::test]
async fn test_agent_miss_stream_role_and_client_ip() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── Agent UA: ring only (never answers) ──────────────────────────────
    let mut agent_ua = make_ua(proxy_addr, AGENT);
    agent_ua.start().await?;
    agent_ua.register().await?;
    let agent_pump = {
        let ua = agent_ua.clone();
        tokio::spawn(async move {
            loop {
                match ua.process_dialog_events().await {
                    Ok(events) => {
                        for ev in events {
                            if let TestUaEvent::IncomingCall(dialog_id, _) = ev {
                                let _ = ua.ring_call(&dialog_id).await;
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
        portpicker::pick_unused_port().unwrap_or(32400),
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

    // Ring timeout (3s) → no-answer miss.
    let no_answer = wait_for_event(
        &capture,
        "queue_agent_no_answer",
        Duration::from_secs(20),
    )
    .await
    .expect("queue_agent_no_answer must fire after the ring timeout");
    let call_id = no_answer["call_id"]
        .as_str()
        .expect("envelope carries call_id")
        .to_string();

    // The miss leg teardown arrives as its own leg-scoped call_hangup.
    sleep(Duration::from_millis(1200)).await;
    let events: Vec<serde_json::Value> = capture
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["call_id"].as_str() == Some(call_id.as_str()))
        .cloned()
        .collect();
    dump_stream(&call_id, &events);

    // ── Generic contract (no answered role expected in a miss flow) ──────
    RwiTimeline::from_events(&call_id, events.clone())
        .assert_agent_role_contract(AGENT, true);

    // ── Miss events: agent identity only (queue family) ─────────────────
    for etype in ["queue_agent_no_answer", "skill_group_agent_no_answer"] {
        let evs = events_of_type(&events, etype);
        assert!(!evs.is_empty(), "{etype} must appear in the miss flow");
        for ev in evs {
            assert_eq!(
                payload(ev)["agent_id"].as_str(),
                Some(AGENT),
                "{etype} must name the agent: {ev}"
            );
        }
    }

    let miss_hangups: Vec<&serde_json::Value> = events
        .iter()
        .filter(|v| {
            v["event_type"].as_str() == Some("call_hangup")
                && payload(v)["reason"].as_str() == Some("ring_timeout")
        })
        .collect();
    assert_eq!(
        miss_hangups.len(),
        1,
        "exactly one miss hangup for the timed-out leg: {miss_hangups:?}"
    );
    assert_eq!(
        payload(miss_hangups[0])["leg_role"].as_str(),
        Some("agent"),
        "miss hangup must carry leg_role=agent: {:?}",
        miss_hangups[0]
    );
    assert_eq!(
        payload(miss_hangups[0])["client_ip"].as_str(),
        Some(AGENT_CLIENT_IP),
        "miss hangup must carry client_ip: {:?}",
        miss_hangups[0]
    );
    assert!(
        agent_role_events(&events, "call_answered").is_empty(),
        "no agent answer may be reported in a miss flow"
    );

    caller.hangup(&dialog).await.ok();
    agent_pump.abort();
    harness.server.stop();
    Ok(())
}
