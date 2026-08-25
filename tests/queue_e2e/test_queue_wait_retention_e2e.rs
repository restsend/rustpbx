//! SIP e2e: skill-group wait retention + RWI `skill_group_*` webhook events.
//!
//! Scenario (user-reported):
//! 1. One agent is Busy on caller1
//! 2. Caller2 transfers to the skill group → must **not** ring the busy agent
//! 3. Caller2 hears wait-retention media (answered + hold)
//! 4. RWI webhook receives `skill_group_call_queued` (reason `all_busy`)
//! 5. Caller2 hangs up → `skill_group_call_abandoned`

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::{
    CcAgentRegistryAdapter, SkillGroupEvent,
};
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::addons::cc::translate_skill_group_event;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueFallbackConfig,
    RouteQueueStrategyConfig, RouteQueueTargetConfig, RouteRule,
};
use rustpbx::rwi::{
    RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler,
};
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

const QUEUE_NUMBER: &str = "9100";
const SKILL_GROUP: &str = "wait_retention_q";
const RING_TIMEOUT_SECS: u64 = 2;

#[derive(Clone, Default)]
struct AgentStats {
    invites: Arc<AtomicUsize>,
    established: Arc<AtomicUsize>,
    answered: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentStats {
    fn invites(&self) -> usize {
        self.invites.load(Ordering::Relaxed)
    }
}

fn queue_proxy_config(port: u16) -> ProxyConfig {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };

    let queue_config = RouteQueueConfig {
        name: Some("wait_retention".to_string()),
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Sequential,
            wait_timeout_secs: Some(RING_TIMEOUT_SECS as u16),
            targets: vec![RouteQueueTargetConfig {
                uri: format!("skill-group:{SKILL_GROUP}"),
                label: None,
            }],
        },
        // Wait retention answers itself when no Idle agent exists.
        accept_immediately: false,
        fallback: Some(RouteQueueFallbackConfig {
            failure_code: Some(486),
            failure_reason: Some("All agents busy".to_string()),
            redirect: None,
        }),
        ..Default::default()
    };
    config
        .queues
        .insert("wait_retention".to_string(), queue_config);

    config.routes = Some(vec![RouteRule {
        name: "route_to_wait_retention_queue".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(QUEUE_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("wait_retention".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    config
}

/// One Idle agent + skill-group event channel drained into RWI → webhook.
async fn start_harness(
    port: u16,
    capture: &WebhookCapture,
) -> Result<(
    E2eTestServer,
    Arc<CcAgentRegistryAdapter>,
    tokio::sync::mpsc::UnboundedReceiver<SkillGroupEvent>,
)> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Wait Retention Q".to_string()),
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

    // Production drain: SkillGroupEvent → translate → gateway → webhook,
    // mirroring CcAddon's adapter bridge. Also mirror to `mirror_rx` for
    // direct SkillGroupEvent assertions.
    let webhook_tx = start_rwi_webhook_handler(LocatorWebhookConfig {
        url: capture.url.clone(),
        events: vec![
            "skill_group_call_queued".to_string(),
            "skill_group_call_abandoned".to_string(),
            "skill_group_agent_assigned".to_string(),
        ],
        headers: None,
        timeout_ms: Some(5000),
    });
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));

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
            username: "caller1".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 3,
            username: "caller2".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ];

    let server = E2eTestServer::start_with_inject(
        queue_proxy_config(port),
        E2eTestServerInject {
            users,
            session_hook: None,
            agent_registry: Some(adapter.clone()),
        },
    )
    .await?;

    Ok((server, adapter, mirror_rx))
}

fn make_ua(proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27000),
        proxy_addr,
    })
}

fn spawn_agent_pump(ua: TestUa, stats: AgentStats) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        match ev {
                            TestUaEvent::IncomingCall(dialog_id, offer) => {
                                stats.invites.fetch_add(1, Ordering::Relaxed);
                                if !stats.answered.swap(true, Ordering::Relaxed) {
                                    let sdp = offer
                                        .as_deref()
                                        .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                        .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                    let _ = ua.answer_call(&dialog_id, Some(sdp)).await;
                                }
                            }
                            TestUaEvent::CallEstablished(_) => {
                                stats.established.fetch_add(1, Ordering::Relaxed);
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
) -> anyhow::Result<rsipstack::dialog::DialogId> {
    tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .map_err(|_| anyhow!("caller did not settle within 15s"))?
        .map_err(|e| anyhow!("caller task failed: {e}"))?
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

/// 1 agent Busy → 2nd caller wait-retains (no re-INVITE) → hangup emits
/// `skill_group_call_queued` + `skill_group_call_abandoned` on the RWI webhook.
#[tokio::test]
async fn test_busy_agent_second_caller_wait_retention_rwi_events() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let capture = WebhookCapture::start().await;
    let port = portpicker::pick_unused_port().unwrap_or(16100);
    let (server, _adapter, mut sg_rx) = start_harness(port, &capture).await?;
    let proxy_addr = server.proxy_addr;

    let mut bob = make_ua(proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_stats = AgentStats::default();
    let bob_pump = spawn_agent_pump(bob.clone(), bob_stats.clone());

    // Caller1 connects to the only agent.
    let mut caller1 = make_ua(proxy_addr, "caller1");
    caller1.start().await?;
    let offer1 = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30100),
        false,
    );
    let call1 = {
        let ua = caller1.clone();
        tokio::spawn(async move { ua.make_call(QUEUE_NUMBER, Some(offer1)).await })
    };
    let dialog1 = settle(call1).await.expect("caller1 should connect via bob");
    sleep(Duration::from_millis(500)).await;
    assert_eq!(bob_stats.invites(), 1, "bob must receive caller1 INVITE");
    assert_eq!(
        bob_stats.established.load(Ordering::Relaxed),
        1,
        "bob must be established with caller1"
    );

    // Drain caller1 assignment events so we only assert on caller2 below.
    while sg_rx.try_recv().is_ok() {}
    {
        let mut events = capture.received.lock().unwrap();
        events.clear();
    }

    // Caller2: all agents busy/ringing → wait retention, no second INVITE.
    let mut caller2 = make_ua(proxy_addr, "caller2");
    caller2.start().await?;
    let offer2 = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30101),
        false,
    );
    let call2 = {
        let ua = caller2.clone();
        tokio::spawn(async move { ua.make_call(QUEUE_NUMBER, Some(offer2)).await })
    };
    let dialog2 = settle(call2)
        .await
        .expect("caller2 should be answered in wait retention (not 486)");

    sleep(Duration::from_millis(800)).await;
    assert_eq!(
        bob_stats.invites(),
        1,
        "wait retention must NOT INVITE the busy/ringing agent for caller2"
    );

    let queued = wait_webhook_event(&capture, "skill_group_call_queued", Duration::from_secs(5))
        .await
        .expect("RWI webhook must receive skill_group_call_queued");
    assert_eq!(
        queued["event"]["skill_group_id"].as_str(),
        Some(SKILL_GROUP),
        "queued skill_group_id: {queued}"
    );
    assert_eq!(
        queued["event"]["reason"].as_str(),
        Some("all_busy"),
        "queued reason must be all_busy: {queued}"
    );

    // Also confirm the adapter-side event (source of the RWI translation).
    let mut saw_adapter_queued = false;
    while let Ok(ev) = sg_rx.try_recv() {
        if let SkillGroupEvent::CallQueued {
            skill_group_id,
            reason,
            ..
        } = ev
        {
            assert_eq!(skill_group_id.as_deref(), Some(SKILL_GROUP));
            assert_eq!(reason, "all_busy");
            saw_adapter_queued = true;
        }
    }
    assert!(
        saw_adapter_queued || queued["event_type"].as_str() == Some("skill_group_call_queued"),
        "adapter CallQueued or webhook queued must be present"
    );

    // Abandon while waiting.
    caller2.hangup(&dialog2).await?;

    let abandoned =
        wait_webhook_event(&capture, "skill_group_call_abandoned", Duration::from_secs(5))
            .await
            .expect("RWI webhook must receive skill_group_call_abandoned");
    assert_eq!(
        abandoned["event"]["skill_group_id"].as_str(),
        Some(SKILL_GROUP),
        "abandoned skill_group_id: {abandoned}"
    );

    // Bob must still not have been rung again.
    assert_eq!(
        bob_stats.invites(),
        1,
        "abandon path must not dial bob"
    );

    // Exactly one abandoned webhook for caller2 (no duplicate on_exit).
    {
        let events = capture.received.lock().unwrap();
        let abandoned_count = events
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("skill_group_call_abandoned"))
            .count();
        assert_eq!(
            abandoned_count, 1,
            "skill_group_call_abandoned must fire once, got {abandoned_count}: {events:?}"
        );
    }

    let _ = caller1.hangup(&dialog1).await;
    bob_pump.abort();
    let _ = bob.stop();
    let _ = caller1.stop();
    let _ = caller2.stop();
    server.stop();
    Ok(())
}
