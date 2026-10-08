//! E2E: agent-leg identity contract over the full SIP + RWI webhook path.
//!
//! Covers the three agent-leg creation scenarios introduced with
//! `LegPurpose` + `leg_role`:
//!
//! 1. **queue → agent** — the agent-leg INVITE carries the ROOT session id as
//!    its SIP Call-ID (first agent-facing leg of the session); the leg-level
//!    `call_ringing` reports `leg_role: "agent"` and the SAME id as
//!    `call_id` / `session_id`; `queue_agent_offered` joins via `leg_id`.
//! 2. **agent-initiated call** — the caller (a registered CC agent) gets its
//!    caller leg stamped, observable as `leg_role: "agent"` on `call_held` /
//!    `call_unheld`, while the customer (callee) leg stays `"callee"`.
//! 3. **agent → BC consult** — `LegAdd(purpose: Consult)` (the exact command
//!    the owner-anchored consult flow sends) dials the first consult leg with
//!    the session id and a follow-up consult with `{session_id}-r2`; an
//!    unregistered target reports `leg_role: "consult"`.

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::AcdConfig;
use rustpbx::addons::cc::acd::AcdEngine;
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::domain::{CallCommand, LegId, LegPurpose};
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
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{
    TestUa, TestUaConfig, TestUaEvent, create_test_sdp, create_test_sdp_answer,
};
use crate::common::webhook_capture::WebhookCapture;

const QUEUE_NUMBER: &str = "9301";
const QUEUE_NAME: &str = "callid_contract_q";
const SKILL_GROUP: &str = "sg_callid_contract";

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "call_held",
    "call_unheld",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_connected",
    "queue_left",
];

/// Call-IDs seen on incoming INVITEs, per UA — filled by the answer pumps.
type SeenCallIds = Arc<Mutex<Vec<String>>>;

fn queue_contract_proxy_config() -> ProxyConfig {
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
            wait_timeout_secs: Some(20),
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
        name: "route_to_callid_contract_queue".to_string(),
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

struct ContractHarness {
    server: E2eTestServer,
    cc_registry: Arc<CcAgentRegistry>,
}

async fn start_harness(capture: &WebhookCapture) -> Result<ContractHarness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("CallId Contract Q".to_string()),
            skills_required: vec!["support".to_string()],
            overflow_groups: vec![],
            sla_target_secs: 30,
            max_wait_secs: 60,
            metadata: None,
        },
    )
    .await
    .unwrap();

    // `bob` is the queue agent AND the agent in the agent-initiated scenario;
    // charlie/dave are plain users (consult targets → leg_role "consult").
    let cc_registry = Arc::new(CcAgentRegistry::with_db(db.clone()));
    cc_registry
        .register("bob".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    cc_registry
        .update_status("bob", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();

    let adapter = Arc::new(CcAgentRegistryAdapter::new(
        cc_registry.clone(),
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: false,
            ..AcdConfig::default()
        })),
        "localhost",
    ));

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

    let mut proxy_config = queue_contract_proxy_config();
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
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 3,
            username: "charlie".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 4,
            username: "dave".to_string(),
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

    Ok(ContractHarness {
        server,
        cc_registry,
    })
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28000),
        proxy_addr,
    })
}

/// Answer every incoming call (180 first — a real phone rings), recording the
/// SIP Call-ID of each received INVITE into `seen`.
fn spawn_answer_pump(ua: TestUa, seen: SeenCallIds) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        if let TestUaEvent::IncomingCall(dialog_id, offer) = ev {
                            seen.lock().unwrap().push(dialog_id.call_id.clone());
                            let _ = ua.ring_call(&dialog_id).await;
                            sleep(Duration::from_millis(200)).await;
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
        sleep(Duration::from_millis(50)).await;
    }
}

/// First webhook event matching `pred`, waiting up to `timeout`.
async fn wait_webhook_matching(
    capture: &WebhookCapture,
    event_type: &str,
    pred: impl Fn(&serde_json::Value) -> bool,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if let Some(ev) = events.iter().find(|v| {
                v["event_type"].as_str() == Some(event_type) && pred(v)
            }) {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn render_timeline(capture: &WebhookCapture, call_id: &str) -> String {
    let events = capture.received.lock().unwrap();
    events
        .iter()
        .filter(|v| v["call_id"].as_str() == Some(call_id))
        .map(|v| {
            let p = if v["event"].is_object() {
                &v["event"]
            } else {
                v
            };
            format!(
                "  {} leg_id={:?} leg_role={:?} agent_id={:?} call_id={:?} session_id={:?}",
                v["event_type"].as_str().unwrap_or(""),
                p["leg_id"].as_str(),
                p["leg_role"].as_str(),
                p["agent_id"].as_str(),
                p["call_id"].as_str(),
                p["session_id"].as_str(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// ── Scenario ①: caller → queue → agent ─────────────────────────────────────
///
/// The agent-leg INVITE must carry the root session id as its SIP Call-ID,
/// and the RWI events must identify the agent leg (`leg_role: "agent"`) with
/// the same id on `call_id` / `session_id` / `queue_agent_offered.leg_id`.
#[tokio::test]
async fn test_queue_agent_leg_callid_equals_session_id_with_leg_role() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_call_ids: SeenCallIds = Arc::new(Mutex::new(Vec::new()));
    let bob_pump = spawn_answer_pump(bob.clone(), bob_call_ids.clone());

    let mut caller = make_ua(proxy_addr, "alice");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30210),
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

    // The active session id — this is the root session id `S`.
    let session_id = harness
        .server
        .wait_for_active_call(Duration::from_secs(5))
        .await
        .expect("queue call must be active");

    // ── CORE: the agent leg's SIP Call-ID == the root session id ─────────
    let agent_call_id = loop {
        let seen = bob_call_ids.lock().unwrap().clone();
        if let Some(id) = seen.first().cloned() {
            break id;
        }
        if tokio::time::Instant::now()
            >= tokio::time::Instant::now() + Duration::from_secs(10)
        {
            panic!("bob never received the agent-leg INVITE");
        }
        sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        agent_call_id.as_str(), session_id.as_str(),
        "agent-leg INVITE Call-ID must equal the root session id.\n timeline:\n{}",
        render_timeline(&capture, &session_id)
    );

    // ── Leg-level call_ringing: leg_role "agent" + one id across the board ─
    let ringing = wait_webhook_matching(
        &capture,
        "call_ringing",
        |v| !v["event"]["leg_id"].is_null(),
        Duration::from_secs(10),
    )
    .await
    .expect("webhook must receive the agent-leg call_ringing");
    let ringing_payload = &ringing["event"];
    assert_eq!(
        ringing_payload["leg_role"].as_str(),
        Some("agent"),
        "agent-leg call_ringing must carry leg_role=agent: {ringing}"
    );
    assert_eq!(
        ringing_payload["agent_id"].as_str(),
        Some("bob"),
        "agent-leg call_ringing must pin the agent: {ringing}"
    );
    assert_eq!(
        ringing_payload["call_id"].as_str(),
        Some(session_id.as_str()),
        "leg event call_id must be the session id: {ringing}"
    );
    assert_eq!(
        ringing_payload["session_id"].as_str(),
        Some(session_id.as_str()),
        "enriched session_id must be the root session id: {ringing}"
    );
    let agent_leg_id = ringing_payload["leg_id"]
        .as_str()
        .expect("leg-level ringing carries leg_id")
        .to_string();

    // ── queue_agent_offered joins via leg_id ─────────────────────────────
    let offered = wait_webhook_event(&capture, "queue_agent_offered", Duration::from_secs(10))
        .await
        .expect("webhook must receive queue_agent_offered");
    assert_eq!(
        offered["event"]["leg_id"].as_str(),
        Some(agent_leg_id.as_str()),
        "queue_agent_offered.leg_id must join with the agent-leg call_ringing: {offered}"
    );

    wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(10))
        .await
        .expect("bob answers → queue_agent_connected");

    // ── Hangup: session-level event still correlates by session_id ───────
    caller.hangup(&dialog).await?;
    let hangup = wait_webhook_event(&capture, "call_hangup", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_hangup");
    assert_eq!(
        hangup["event"]["session_id"].as_str(),
        Some(session_id.as_str()),
        "call_hangup must correlate by the root session id: {hangup}"
    );

    bob_pump.abort();
    let _ = bob.stop();
    let _ = caller.stop();
    harness.server.stop();
    Ok(())
}

/// ── Scenario ③: agent-initiated call ───────────────────────────────────────
///
/// `bob` (a registered CC agent) dials a plain extension. The caller leg is
/// the agent leg: its RWI events (`call_held` / `call_unheld` on the caller
/// leg) must report `leg_role: "agent"` while the customer (callee) leg stays
/// `"callee"`, and everything correlates by the root session id.
#[tokio::test]
async fn test_agent_initiated_call_caller_leg_reports_agent_role() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    let mut alice = make_ua(proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    let alice_call_ids: SeenCallIds = Arc::new(Mutex::new(Vec::new()));
    let alice_pump = spawn_answer_pump(alice.clone(), alice_call_ids.clone());

    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    // bob is the CALLER — his pump is still required so the UA answers the
    // server-initiated hold re-INVITE (otherwise the Hold command times out).
    let bob_call_ids: SeenCallIds = Arc::new(Mutex::new(Vec::new()));
    let bob_pump = spawn_answer_pump(bob.clone(), bob_call_ids.clone());
    sleep(Duration::from_millis(300)).await;

    // bob (agent) → alice (customer), plain extension call.
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30220),
        false,
    );
    let bob_offer_sdp = offer.clone();
    let call = {
        let ua = bob.clone();
        tokio::spawn(async move { ua.make_call("alice", Some(offer)).await })
    };
    let dialog = tokio::time::timeout(Duration::from_secs(15), call)
        .await
        .map_err(|_| anyhow!("bob's call did not settle within 15s"))?
        .map_err(|e| anyhow!("bob's call task failed: {e}"))?
        .expect("agent call established");

    // The UA's re-INVITE auto-answer replies with the dialog's stored answer
    // SDP; the caller-side dialog has none unless we set it. Without it the
    // hold re-INVITE gets a bodyless 200 and times out.
    bob.set_answer_sdp(&dialog, &bob_offer_sdp).await;

    let session_id = harness
        .server
        .wait_for_active_call(Duration::from_secs(5))
        .await
        .expect("agent-initiated call must be active");

    // The customer's UA must have seen the customer (callee) leg — a plain
    // random proxy Call-ID, NOT the session id (only agent-facing dials are
    // session-aligned; this leg is the dialplan B-leg).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let callee_call_id = loop {
        let seen = alice_call_ids.lock().unwrap().clone();
        if let Some(id) = seen.first().cloned() {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "alice never received the call"
        );
        sleep(Duration::from_millis(50)).await;
    };
    assert_ne!(
        callee_call_id.as_str(), session_id.as_str(),
        "dialplan B-leg keeps its own Call-ID (not session-aligned)"
    );

    // call_created correlates by the root session id.
    let created = wait_webhook_event(&capture, "call_created", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_created");
    assert_eq!(
        created["call_id"].as_str(),
        Some(session_id.as_str()),
        "call_created call_id must be the root session id: {created}"
    );

    // The customer (callee) leg rings with the plain "callee" role.
    let ringing = wait_webhook_matching(
        &capture,
        "call_ringing",
        |v| v["event"]["leg_id"].as_str() == Some("callee"),
        Duration::from_secs(10),
    )
    .await
    .expect("webhook must receive the callee-leg call_ringing");
    assert_eq!(
        ringing["event"]["leg_role"].as_str(),
        Some("callee"),
        "customer leg must stay leg_role=callee: {ringing}"
    );

    // Wait for the session-level answer (200 OK → ACK confirmed) before
    // holding — the caller dialog must be Confirmed or rsipstack refuses the
    // re-INVITE (Ok(None) → "re-INVITE timed out").
    wait_webhook_event(&capture, "call_answered", Duration::from_secs(10))
        .await
        .expect("webhook must receive the session-level call_answered");
    sleep(Duration::from_millis(500)).await;

    // Hold the CALLER leg (bob = agent) via the production command path and
    // observe `call_held { leg_id: "caller", leg_role: "agent" }`. Retry a
    // few times in case the dialog confirmation races the first attempt.
    let handle = harness
        .server
        .registry
        .get_handle(&session_id)
        .expect("session handle must be registered");
    let mut held = None;
    for _ in 0..5 {
        let _ = handle.send_command(CallCommand::Hold {
            leg_id: LegId::from("caller"),
            music: None,
        });
        if let Some(ev) = wait_webhook_matching(
            &capture,
            "call_held",
            |v| v["event"]["leg_id"].as_str() == Some("caller"),
            Duration::from_secs(3),
        )
        .await
        {
            held = Some(ev);
            break;
        }
        sleep(Duration::from_millis(400)).await;
    }
    let held = held.expect("webhook must receive call_held for the caller leg");
    assert_eq!(
        held["event"]["leg_role"].as_str(),
        Some("agent"),
        "agent-initiated caller leg must report leg_role=agent: {held}"
    );
    assert_eq!(
        held["event"]["call_id"].as_str(),
        Some(session_id.as_str()),
        "call_held correlates by the root session id (registry session_id={:?}): {held}",
        session_id
    );

    let hangup_result = bob.hangup(&dialog).await;
    if let Err(e) = &hangup_result {
        tracing::warn!("bob hangup failed: {e}");
    }
    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_hangup");

    bob_pump.abort();
    alice_pump.abort();
    let _ = bob.stop();
    let _ = alice.stop();
    harness.server.stop();
    Ok(())
}

/// ── Scenario ②: agent → BC consult ─────────────────────────────────────────
///
/// Establish A→B, then send the exact owner-anchored consult command
/// (`LegAdd` with `purpose: Consult`, as `cluster_owner::consult_start`
/// does). The first consult leg dials with the session id as its Call-ID; a
/// follow-up consult (after removing the first) gets `{session_id}-r2`.
/// charlie/dave are NOT registered agents → `leg_role: "consult"`.
#[tokio::test]
async fn test_consult_leg_callid_session_aligned_and_leg_role() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let harness = start_harness(&capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let bob_call_ids: SeenCallIds = Arc::new(Mutex::new(Vec::new()));
    let bob_pump = spawn_answer_pump(bob.clone(), bob_call_ids.clone());

    let mut charlie = make_ua(proxy_addr, "charlie");
    charlie.start().await?;
    charlie.register().await?;
    let charlie_call_ids: SeenCallIds = Arc::new(Mutex::new(Vec::new()));
    let charlie_pump = spawn_answer_pump(charlie.clone(), charlie_call_ids.clone());

    let mut dave = make_ua(proxy_addr, "dave");
    dave.start().await?;
    dave.register().await?;
    let dave_call_ids: SeenCallIds = Arc::new(Mutex::new(Vec::new()));
    let dave_pump = spawn_answer_pump(dave.clone(), dave_call_ids.clone());

    let mut alice = make_ua(proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30230),
        false,
    );
    let call = {
        let ua = alice.clone();
        tokio::spawn(async move { ua.make_call("bob", Some(offer)).await })
    };
    tokio::time::timeout(Duration::from_secs(15), call)
        .await
        .map_err(|_| anyhow!("A→B call did not settle within 15s"))?
        .map_err(|e| anyhow!("A→B call failed: {e}"))?
        .expect("A→B established");

    // Wait for bob's UA to actually receive the established B-leg INVITE so
    // the "callee" leg exists before consulting.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while bob_call_ids.lock().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "bob never received the A→B INVITE"
        );
        sleep(Duration::from_millis(50)).await;
    }
    sleep(Duration::from_millis(500)).await;

    let session_id = harness
        .server
        .wait_for_active_call(Duration::from_secs(5))
        .await
        .expect("A→B call must be active");
    let handle = harness
        .server
        .registry
        .get_handle(&session_id)
        .expect("session handle must be registered");

    // ── Consult #1 → charlie: first agent-facing leg dials with `S` ──────
    handle
        .send_command(CallCommand::LegAdd {
            source_leg: Some(LegId::from("callee")),
            target: "charlie".to_string(),
            leg_id: Some(LegId::new("consult-1")),
            headers: vec![],
            purpose: Some(LegPurpose::Consult),
        })
        .map_err(|e| anyhow!("consult LegAdd failed: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let charlie_call_id = loop {
        let seen = charlie_call_ids.lock().unwrap().clone();
        if let Some(id) = seen.first().cloned() {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "charlie never received the consult INVITE"
        );
        sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        charlie_call_id.as_str(), session_id.as_str(),
        "first consult leg INVITE Call-ID must equal the root session id.\n timeline:\n{}",
        render_timeline(&capture, &session_id)
    );

    // charlie is NOT a registered agent → leg_role "consult" on leg events.
    let consult_ringing = wait_webhook_matching(
        &capture,
        "call_ringing",
        |v| v["event"]["leg_id"].as_str() == Some("consult-1"),
        Duration::from_secs(10),
    )
    .await
    .expect("webhook must receive the consult-leg call_ringing");
    assert_eq!(
        consult_ringing["event"]["leg_role"].as_str(),
        Some("consult"),
        "unregistered consult target must report leg_role=consult: {consult_ringing}"
    );
    assert_eq!(
        consult_ringing["event"]["call_id"].as_str(),
        Some(session_id.as_str()),
        "consult leg events correlate by the root session id: {consult_ringing}"
    );

    // ── Consult #2 → dave: second agent-facing leg dials with `{S}-r2` ───
    handle
        .send_command(CallCommand::LegRemove {
            leg_id: LegId::from("consult-1"),
        })
        .map_err(|e| anyhow!("consult leg remove failed: {e}"))?;
    sleep(Duration::from_millis(500)).await;

    handle
        .send_command(CallCommand::LegAdd {
            source_leg: Some(LegId::from("callee")),
            target: "dave".to_string(),
            leg_id: Some(LegId::new("consult-2")),
            headers: vec![],
            purpose: Some(LegPurpose::Consult),
        })
        .map_err(|e| anyhow!("consult #2 LegAdd failed: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let dave_call_id = loop {
        let seen = dave_call_ids.lock().unwrap().clone();
        if let Some(id) = seen.first().cloned() {
            break id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "dave never received the consult INVITE"
        );
        sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        dave_call_id.as_str(),
        format!("{}-r2", session_id).as_str(),
        "follow-up consult leg must use the retry suffix `-r2`.\n timeline:\n{}",
        render_timeline(&capture, &session_id)
    );

    let consult2_ringing = wait_webhook_matching(
        &capture,
        "call_ringing",
        |v| v["event"]["leg_id"].as_str() == Some("consult-2"),
        Duration::from_secs(10),
    )
    .await
    .expect("webhook must receive the consult-2 call_ringing");
    assert_eq!(
        consult2_ringing["event"]["leg_role"].as_str(),
        Some("consult"),
        "consult-2 must also report leg_role=consult: {consult2_ringing}"
    );

    bob_pump.abort();
    charlie_pump.abort();
    dave_pump.abort();
    let _ = bob.stop();
    let _ = alice.stop();
    let _ = charlie.stop();
    let _ = dave.stop();
    harness.server.stop();
    Ok(())
}
