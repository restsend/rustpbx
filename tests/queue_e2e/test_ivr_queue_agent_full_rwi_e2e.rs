//! Full-call-chain SIP e2e with complete RWI webhook event capture:
//!
//! `caller → IVR (press 1) → queue (wait retention, 排队) → agent assigned
//! (分配) & answered → hangup (结束)`.
//!
//! Asserts the RWI webhook receives the expected event sequence for the
//! whole chain (one caller, one agent that is Busy at first so the call
//! actually queues, then goes Idle so wait retention assigns it):
//!
//! 1. `call_created` — session lifecycle
//! 2. `ivr_node_entered` / `ivr_node_exited` — tree IVR menu flow
//! 3. `queue_joined` — broadcast by `start_queue_app` BEFORE agent resolution
//!    (strict ordering: it must precede every ACD event)
//! 4. `skill_group_candidates_found` + `skill_group_call_queued` (reason
//!    `all_busy`) — ACD adapter, during target resolution
//! 5. `skill_group_agent_assigned` — after the agent goes Idle (wait
//!    retention poll re-resolves)
//! 6. `call_ringing` — agent leg 180 (dynamic-leg path)
//! 7. `queue_agent_offered` — agent leg ringing
//! 8. `queue_agent_connected` — agent answered
//! 9. `queue_left` (connected) — caller bridged to the agent
//! 10. `call_hangup` — call teardown

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
use crate::common::test_ua::{
    TestUa, TestUaConfig, TestUaEvent, create_test_sdp, create_test_sdp_answer,
};
use crate::common::webhook_capture::WebhookCapture;

const IVR_NUMBER: &str = "9200";
const QUEUE_NAME: &str = "full_chain_q";
const SKILL_GROUP: &str = "sg_full_chain";

/// Every RWI event type the full chain is expected to produce.
const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_connected",
    "queue_left",
    "skill_group_call_queued",
    "skill_group_agent_assigned",
    "skill_group_candidates_found",
    "ivr_node_entered",
    "ivr_node_exited",
];

fn ivr_toml() -> String {
    // The greeting file does not exist → tree IVR skips playback and waits
    // for DTMF immediately (no TTS / audio dependency in CI).
    format!(
        r#"
[ivr]
name = "full-chain-ivr"

[ivr.root]
greeting = "sounds/definitely-missing-menu.wav"
timeout_ms = 10000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = {{ type = "transfer", target = "queue:{QUEUE_NAME}" }}

[[ivr.root.entries]]
key = "9"
action = {{ type = "hangup" }}
"#
    )
}

fn full_chain_proxy_config(ivr_file: &std::path::Path) -> ProxyConfig {
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
            wait_timeout_secs: Some(30),
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
        name: "route_to_full_chain_ivr".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(IVR_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": ivr_file.to_string_lossy(),
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);

    config
}

struct FullChainHarness {
    server: E2eTestServer,
    /// Shared CC registry — flip `bob` to Idle to trigger assignment.
    cc_registry: Arc<CcAgentRegistry>,
    sg_rx: tokio::sync::mpsc::UnboundedReceiver<SkillGroupEvent>,
}

/// Server + skill-group + adapter drained into the shared RWI gateway whose
/// webhook handler forwards every chain event to `capture`.
async fn start_harness(port: u16, capture: &WebhookCapture) -> Result<FullChainHarness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Full Chain Q".to_string()),
            skills_required: vec!["support".to_string()],
            overflow_groups: vec![],
            sla_target_secs: 30,
            max_wait_secs: 120,
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
    // Agent starts BUSY: the caller must actually queue first (排队), then
    // the test flips bob to Idle so wait retention assigns him (分配).
    // The presence state machine requires offline → idle → busy.
    cc_registry
        .update_status("bob", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();
    cc_registry
        .update_status(
            "bob",
            rustpbx::addons::cc::agent::AgentStatus::Busy {
                call_id: "warmup".to_string(),
                since: std::time::Instant::now(),
            },
        )
        .await
        .unwrap();

    let (sg_tx, sg_rx) = tokio::sync::mpsc::unbounded_channel::<SkillGroupEvent>();
    let harness_registry = cc_registry.clone();
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

    // Shared gateway: session lifecycle (call_*), app events (queue_* /
    // ivr_*) and the skill-group bridge all fan out here; the webhook
    // handler forwards everything the harness subscribed to.
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(LocatorWebhookConfig {
            url: capture.url.clone(),
            events: WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
            headers: None,
            timeout_ms: Some(5000),
        }));
        gw
    }));

    // Production drain: SkillGroupEvent → translate → gateway. Events are
    // mirrored back so the test can also assert on the adapter-level events.
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

    // IVR definition: written to a temp file and referenced by absolute path
    // from the route's app_params (builtin app factory reads `file`).
    let ivr_path = std::env::temp_dir().join(format!(
        "full-chain-ivr-{}.toml",
        portpicker::pick_unused_port().unwrap_or(42000)
    ));
    std::fs::write(&ivr_path, ivr_toml())?;

    let mut proxy_config = full_chain_proxy_config(&ivr_path);
    // The base builder overrides ports; keep our IVR dir config intact.
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
            users,
            session_hook: None,
            agent_registry: Some(adapter.clone()),
            rwi_gateway: Some(gateway),
        },
    )
    .await?;

    Ok(FullChainHarness {
        server,
        cc_registry: harness_registry,
        sg_rx: mirror_rx,
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

fn spawn_agent_pump(
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
                                // A real phone rings (180) before the agent
                                // picks up; the session turns the 180 into
                                // `call_ringing` / `queue_agent_offered`.
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

async fn settle(
    handle: tokio::task::JoinHandle<Result<rsipstack::dialog::DialogId>>,
) -> Result<rsipstack::dialog::DialogId> {
    tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .map_err(|_| anyhow!("caller did not settle within 15s"))?
        .map_err(|e| anyhow!("caller task failed: {e}"))?
}

/// Wait until the webhook capture has seen `event_type`; returns the payload.
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

/// Full chain: caller → IVR menu (press 1) → queue wait retention → agent
/// assigned & answered → hangup. Verifies the RWI webhook receives the
/// complete expected event set with the key field assertions per event.
#[tokio::test]
async fn test_full_chain_ivr_queue_agent_rwi_webhook_events() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let port = portpicker::pick_unused_port().unwrap_or(17100);
    let harness = start_harness(port, &capture).await?;
    let proxy_addr = harness.server.proxy_addr;

    // ── Agent leg ────────────────────────────────────────────────────────
    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(300)).await;

    let invites = Arc::new(AtomicUsize::new(0));
    let established = Arc::new(AtomicUsize::new(0));
    let bob_pump = spawn_agent_pump(bob.clone(), invites.clone(), established.clone());

    // ── Caller → IVR ─────────────────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30200),
        false,
    );
    let call = {
        let ua = caller.clone();
        tokio::spawn(async move { ua.make_call(IVR_NUMBER, Some(offer)).await })
    };
    let dialog = settle(call).await.expect("caller should reach the IVR");
    sleep(Duration::from_millis(600)).await;

    // The greeting file is missing → IVR waits for DTMF immediately.
    let entered = wait_webhook_event(&capture, "ivr_node_entered", Duration::from_secs(5))
        .await
        .expect("webhook must receive ivr_node_entered");
    assert_eq!(
        entered["event"]["node_type"].as_str(),
        Some("menu"),
        "ivr_node_entered: {entered}"
    );

    // ── Press 1 → transfer to the queue ──────────────────────────────────
    caller.send_dtmf_info(&dialog, "1").await?;
    sleep(Duration::from_millis(500)).await;

    wait_webhook_event(&capture, "ivr_node_exited", Duration::from_secs(5))
        .await
        .expect("webhook must receive ivr_node_exited after the key press");

    // ── Queue wait retention (排队): agent is Busy → queued all_busy ─────
    let queued = wait_webhook_event(&capture, "skill_group_call_queued", Duration::from_secs(5))
        .await
        .expect("webhook must receive skill_group_call_queued");
    assert_eq!(
        queued["event"]["skill_group_id"].as_str(),
        Some(SKILL_GROUP),
        "queued: {queued}"
    );
    assert_eq!(
        queued["event"]["reason"].as_str(),
        Some("all_busy"),
        "agent is Busy → reason must be all_busy: {queued}"
    );
    wait_webhook_event(&capture, "queue_joined", Duration::from_secs(5))
        .await
        .expect("webhook must receive queue_joined");

    // ── Agent goes Idle → wait retention assigns him (分配) ──────────────
    // Presence state machine: busy → wrapup → idle.
    harness
        .cc_registry
        .update_status(
            "bob",
            rustpbx::addons::cc::agent::AgentStatus::Wrapup {
                call_id: "warmup".to_string(),
                since: std::time::Instant::now(),
            },
        )
        .await?;
    harness
        .cc_registry
        .update_status("bob", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await?;

    let assigned = wait_webhook_event(
        &capture,
        "skill_group_agent_assigned",
        Duration::from_secs(15),
    )
    .await
    .expect("webhook must receive skill_group_agent_assigned after agent goes Idle");
    assert_eq!(
        assigned["event"]["agent_id"].as_str(),
        Some("bob"),
        "assigned: {assigned}"
    );

    // Agent leg rings → offered → answered → connected → left(connected).
    let ringing = wait_webhook_event(&capture, "call_ringing", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_ringing for the agent leg (dynamic-leg path)");
    assert!(
        ringing["event"]["call_id"].is_string(),
        "call_ringing: {ringing}"
    );

    wait_webhook_event(&capture, "queue_agent_offered", Duration::from_secs(10))
        .await
        .expect("webhook must receive queue_agent_offered");
    wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(10))
        .await
        .expect("bob answers automatically → queue_agent_connected");
    let left = wait_webhook_event(&capture, "queue_left", Duration::from_secs(10))
        .await
        .expect("webhook must receive queue_left");
    assert_eq!(
        left["event"]["reason"].as_str(),
        Some("connected"),
        "queue_left must be reason=connected, got: {left}"
    );

    assert_eq!(
        invites.load(Ordering::Relaxed),
        1,
        "bob must be dialed exactly once (after going Idle)"
    );
    // `CallEstablished` is delivered to the UA pump slightly after the
    // queue connects — poll briefly instead of asserting immediately.
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

    // ── End the call (结束) ───────────────────────────────────────────────
    caller.hangup(&dialog).await?;
    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(10))
        .await
        .expect("webhook must receive call_hangup after the caller hangs up");

    // ── Sequence sanity: the chain must appear in a coherent order ───────
    {
        let events = capture.received.lock().unwrap();
        let types: Vec<&str> = events
            .iter()
            .filter_map(|v| v["event_type"].as_str())
            .collect();
        let pos = |name: &str| types.iter().position(|t| *t == name);

        let created = pos("call_created").expect("call_created must be present");
        let node_entered = pos("ivr_node_entered").expect("ivr_node_entered must be present");
        let node_exited = pos("ivr_node_exited").expect("ivr_node_exited must be present");
        let joined = pos("queue_joined").expect("queue_joined must be present");
        let sg_queued =
            pos("skill_group_call_queued").expect("skill_group_call_queued must be present");
        let assigned_pos =
            pos("skill_group_agent_assigned").expect("agent_assigned must be present");
        let ringing_pos = pos("call_ringing").expect("call_ringing must be present");
        let offered = pos("queue_agent_offered").expect("queue_agent_offered must be present");
        let connected =
            pos("queue_agent_connected").expect("queue_agent_connected must be present");
        let left_pos = pos("queue_left").expect("queue_left must be present");

        assert!(
            created < node_entered,
            "call_created before ivr_node_entered: {types:?}"
        );
        assert!(
            node_entered < node_exited,
            "menu entered before exited: {types:?}"
        );
        assert!(
            node_exited < joined,
            "IVR exit before queue join: {types:?}"
        );
        // Strict ordering: `queue_joined` is broadcast by `start_queue_app`
        // BEFORE agent resolution, so it must precede every skill-group/ACD
        // event of this queue entry.
        let candidates_pos = pos("skill_group_candidates_found")
            .expect("skill_group_candidates_found must be present");
        assert!(
            joined < candidates_pos,
            "queue_joined before candidates_found: {types:?}"
        );
        assert!(
            joined < sg_queued,
            "queue_joined before skill_group_call_queued: {types:?}"
        );
        assert!(
            sg_queued < assigned_pos,
            "queued before assigned: {types:?}"
        );
        assert!(
            joined < assigned_pos,
            "queue_joined before assigned: {types:?}"
        );
        assert!(
            assigned_pos < ringing_pos,
            "assigned before agent leg rings: {types:?}"
        );
        assert!(
            ringing_pos <= offered,
            "agent leg ringing before offered: {types:?}"
        );
        assert!(offered < connected, "offered before connected: {types:?}");
        assert!(
            connected < left_pos,
            "connected before queue_left: {types:?}"
        );
    }

    bob_pump.abort();
    let _ = bob.stop();
    let _ = caller.stop();
    harness.server.stop();
    Ok(())
}
