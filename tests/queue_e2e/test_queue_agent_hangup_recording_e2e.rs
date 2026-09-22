//! e2e regression for the agent-hangup-FIRST path:
//!
//! `caller → queue → agent answers → AGENT hangs up while the caller is
//! still online`.
//!
//! The sibling full-chain e2e only covers caller-first hangup, which never
//! enters `handle_start_return_app` → `on_agent_disconnected`. Production
//! incident (2026-09-22): with `[recording] record_on_agent_connect` armed
//! (the cc.toml default), the agent recording segment stop probed the
//! recorder via `QueryRecorderStatus` — a request/reply command answered by
//! the very session loop that was blocked calling the hook. The loop froze
//! for ~11 s (2 × 5 s probe timeouts): the caller BYE was never sent (the
//! customer heard dead air and hung up themselves) and the recorder kept
//! rolling, so the recording outlived the call (25.2 s recorded vs 18.7 s of
//! talk).
//!
//! Unlike the older queue e2e, the CC session hook here is armed with the
//! production recording default (`CcConfig::default()` →
//! `record_on_agent_connect = true`) so the buggy path actually runs.
//!
//! Pins three properties:
//! 1. the caller receives BYE promptly (< 5 s) after the agent hangup;
//! 2. `record_stopped` fires with a duration that stays inside the agent
//!    talk window — the segment must not grow past the call;
//! 3. `call_hangup` attributes the hangup to the callee.

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::cc_call_session_hook::CcCallSessionHook;
use rustpbx::addons::cc::config::CcConfig;
use rustpbx::addons::cc::metrics::MetricsCollector;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::proxy::proxy_call::session_hooks::CallSessionHook;
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{
    TestUa, TestUaConfig, TestUaEvent, create_test_sdp, create_test_sdp_answer,
};
use crate::common::webhook_capture::WebhookCapture;

const QUEUE_NUMBER: &str = "9300";
const QUEUE_NAME: &str = "agent_hangup_q";
const SKILL_GROUP: &str = "sg_agent_hangup";

/// How long the "agent" stays in the talk window before hanging up FIRST.
const AGENT_TALK_TIME: Duration = Duration::from_millis(1500);

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_ringing",
    "call_answered",
    "call_hangup",
    "queue_joined",
    "queue_agent_offered",
    "queue_agent_connected",
    "queue_left",
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

    config.queues.insert(
        QUEUE_NAME.to_string(),
        RouteQueueConfig {
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
            ..Default::default()
        },
    );

    // Direct queue route — no IVR hop; the queue app itself answers the
    // caller (auto_answer) and dials the idle agent immediately.
    config.routes = Some(vec![RouteRule {
        name: "route_to_agent_hangup_queue".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(QUEUE_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("queue".to_string()),
            queue: Some(QUEUE_NAME.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    config
}

struct HangupFirstHarness {
    server: E2eTestServer,
}

/// Server + CC registry + CC session hook armed with the production
/// recording default (`record_on_agent_connect = true`).
async fn start_harness(port: u16, capture: &WebhookCapture) -> Result<HangupFirstHarness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Agent Hangup Q".to_string()),
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

    // The hook's call registry is late-bound: the container is filled with
    // the live server registry right after start (mirrors how CcAddon
    // initializes it in production).
    let call_registry_container: Arc<
        tokio::sync::RwLock<Option<Arc<ActiveProxyCallRegistry>>>,
    > = Arc::new(tokio::sync::RwLock::new(None));
    let session_hook = CcCallSessionHook::new(
        cc_registry.clone(),
        Arc::new(MetricsCollector::new()),
    )
    // Production default: `[recording] record_on_agent_connect = true`.
    .with_cc_config(Arc::new(CcConfig::default()))
    .with_call_registry_shared(call_registry_container.clone());

    let server = E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            users,
            session_hook: Some(Arc::new(session_hook)),
            agent_registry: Some(adapter),
            rwi_gateway: Some(gateway),
        },
    )
    .await?;
    *call_registry_container.write().await = Some(server.registry.clone());

    Ok(HangupFirstHarness { server })
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(29000),
        proxy_addr,
    })
}

fn ms_since(epoch: &Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

/// Agent UA pump: answers every INVITE, talks for [`AGENT_TALK_TIME`], then
/// hangs up FIRST — the caller stays online. Records the hangup instant.
fn spawn_agent_pump(
    ua: TestUa,
    epoch: Instant,
    established: Arc<AtomicUsize>,
    hangup_at_ms: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
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
                                    .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                    .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                            }
                            TestUaEvent::CallEstablished(dialog_id) => {
                                established.fetch_add(1, Ordering::Relaxed);
                                sleep(AGENT_TALK_TIME).await;
                                hangup_at_ms.store(ms_since(&epoch), Ordering::Relaxed);
                                let _ = ua.hangup(&dialog_id).await;
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

/// Caller UA pump: only records when the dialog terminates (BYE received).
fn spawn_caller_pump(ua: TestUa, epoch: Instant, terminated_at_ms: Arc<AtomicU64>) {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        if matches!(ev, TestUaEvent::CallTerminated(_)) {
                            terminated_at_ms.store(ms_since(&epoch), Ordering::Relaxed);
                        }
                    }
                }
                Err(_) => break,
            }
            sleep(Duration::from_millis(30)).await;
        }
    });
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

/// Agent answers, then hangs up FIRST while the caller stays online. The
/// caller must get the BYE promptly and the agent recording segment must
/// stop within the talk window instead of growing past the call.
#[tokio::test]
async fn test_agent_hangup_first_propagates_bye_and_stops_recording() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let port = portpicker::pick_unused_port().unwrap_or(17300);
    let harness = start_harness(port, &capture).await?;
    let proxy_addr = harness.server.proxy_addr;
    let epoch = Instant::now();

    // ── Agent leg ────────────────────────────────────────────────────────
    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(300)).await;

    let established = Arc::new(AtomicUsize::new(0));
    let hangup_at_ms = Arc::new(AtomicU64::new(0));
    let bob_pump = spawn_agent_pump(
        bob.clone(),
        epoch,
        established.clone(),
        hangup_at_ms.clone(),
    );

    // ── Caller → queue ───────────────────────────────────────────────────
    let mut caller = make_ua(proxy_addr, "caller");
    caller.start().await?;
    let terminated_at_ms = Arc::new(AtomicU64::new(0));
    spawn_caller_pump(caller.clone(), epoch, terminated_at_ms.clone());

    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(31200),
        false,
    );
    let call = {
        let ua = caller.clone();
        tokio::spawn(async move { ua.make_call(QUEUE_NUMBER, Some(offer)).await });
    };
    // Queue answers the caller (auto_answer) and dials the idle agent.
    wait_webhook_event(&capture, "queue_agent_connected", Duration::from_secs(15))
        .await
        .expect("bob must be dialed and answer (queue_agent_connected)");

    let established_deadline = Instant::now() + Duration::from_secs(5);
    while established.load(Ordering::Relaxed) == 0 && Instant::now() < established_deadline {
        sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        established.load(Ordering::Relaxed),
        1,
        "bob must be established with the caller"
    );

    // ── Agent hangs up FIRST — caller still online ───────────────────────
    let hangup_deadline = Instant::now() + Duration::from_secs(10);
    while hangup_at_ms.load(Ordering::Relaxed) == 0 && Instant::now() < hangup_deadline {
        sleep(Duration::from_millis(50)).await;
    }
    let agent_hangup_at = hangup_at_ms.load(Ordering::Relaxed);
    assert!(
        agent_hangup_at > 0,
        "agent pump must hang up after the talk window"
    );

    // 1. Caller BYE must propagate promptly. The incident stalled the
    //    session loop ~11 s here (recorder-probe deadlock) and the caller
    //    never got a BYE until they hung up themselves.
    let terminated_deadline = Instant::now() + Duration::from_secs(12);
    while terminated_at_ms.load(Ordering::Relaxed) == 0 && Instant::now() < terminated_deadline {
        sleep(Duration::from_millis(50)).await;
    }
    let caller_terminated_at = terminated_at_ms.load(Ordering::Relaxed);
    assert!(
        caller_terminated_at > 0,
        "caller must receive BYE after the agent hangs up"
    );
    let bye_latency_ms = caller_terminated_at.saturating_sub(agent_hangup_at);
    assert!(
        bye_latency_ms < 5_000,
        "caller BYE took {bye_latency_ms}ms after the agent hangup — \
         recorder-probe stall regression (the caller hears dead air)"
    );

    // 2. The agent recording segment must stop within the talk window —
    //    with the stall it kept recording through the dead air (~13 s).
    let stopped = wait_webhook_event(&capture, "record_stopped", Duration::from_secs(10))
        .await
        .expect("webhook must receive record_stopped for the agent segment");
    let recorded_secs = stopped["event"]["duration_secs"]
        .as_i64()
        .ok_or_else(|| anyhow!("record_stopped missing duration_secs: {stopped}"))?;
    let talk_budget_secs = AGENT_TALK_TIME.as_secs() + 5;
    assert!(
        recorded_secs <= talk_budget_secs as i64,
        "agent recording ran {recorded_secs}s — grew past the ~{}s talk window; \
         the recorder kept rolling after the agent hung up",
        AGENT_TALK_TIME.as_secs()
    );
    assert!(
        stopped["event"]["filename"]
            .as_str()
            .is_some_and(|f| f.ends_with("_bob.wav")),
        "record_stopped must be the agent segment owned by bob: {stopped}"
    );

    // 3. The call end is attributed to the agent. The dynamic-leg
    //    disconnect fires a per-leg call_hangup first (hangup_by=null,
    //    direction=outbound); the session-level event carries the
    //    callee attribution.
    let hangup_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut session_hangup: Option<serde_json::Value> = None;
    while tokio::time::Instant::now() < hangup_deadline {
        if let Some(ev) = capture
            .received
            .lock()
            .unwrap()
            .iter()
            .find(|v| {
                v["event_type"].as_str() == Some("call_hangup")
                    && v["event"]["hangup_by"].as_str().is_some()
            })
            .cloned()
        {
            session_hangup = Some(ev);
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let hangup = session_hangup
        .ok_or_else(|| anyhow!("no session-level call_hangup with hangup_by attribution"))?;
    // Attribution vocabulary: the connected callee IS the agent, so
    // `hangup_by` is "agent" while the raw SIP reason is "callee".
    assert_eq!(
        hangup["event"]["hangup_by"].as_str(),
        Some("agent"),
        "agent hung up first → hangup_by must be agent: {hangup}"
    );
    assert_eq!(
        hangup["event"]["reason"].as_str(),
        Some("callee"),
        "agent hung up first → reason must be callee: {hangup}"
    );

    bob_pump.abort();
    let _ = bob.stop();
    let _ = caller.stop();
    harness.server.stop();
    Ok(())
}
