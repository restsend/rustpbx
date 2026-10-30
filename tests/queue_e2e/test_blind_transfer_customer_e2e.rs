//! e2e: CC API single-step blind transfer (`POST /cc/calls/{id}/transfer` →
//! `CallCommand::BlindTransferCustomer`) on a queue-dispatched call.
//!
//! Production shape: customer → queue → agent answers (queue dials the agent
//! into a DYNAMIC `LegPurpose::Agent` leg bridged to the caller), then the
//! agent's console transfers the CUSTOMER via the CTI API. Three targets:
//!
//! 1. registered extension — locator hit, independent target leg, agent BYE
//!    at dispatch ("发起即释放"), customer + target survive, hangup cascade;
//! 2. bare number routed to a QUEUE — in-session hand-off (QueueApp), no
//!    dumb-dialled INVITE, no second session (the pre-unification behavior
//!    re-entered the queue as a NEW inbound call);
//! 3. bare number routed FORWARD to a trunk — the INVITE lands on the trunk
//!    destination (route match + trunk selection), transfer event stays sip.
//!
//! Guards the `route_transfer_target_leg` unification in
//! `handle_customer_blind_transfer_sip`: the API previously dropped
//! queue/application route results while the SIP REFER path routed them.

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::cc_call_session_hook::CcCallSessionHook;
use rustpbx::addons::cc::config::CcConfig;
use rustpbx::addons::cc::metrics::MetricsCollector;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::domain::CallCommand;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::proxy::routing::{
    DestConfig, MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig,
    RouteQueueStrategyConfig, RouteQueueTargetConfig, RouteRule, TrunkConfig,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use rsipstack::dialog::DialogId;
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent, create_test_sdp};
use crate::common::webhook_capture::WebhookCapture;

const QUEUE_NUMBER: &str = "9300";
const SOURCE_QUEUE: &str = "blind_src_q";
const SKILL_GROUP: &str = "sg_blind_transfer";
const ROUTED_QUEUE_NUMBER: &str = "8000";
const ROUTED_QUEUE_NAME: &str = "blind_route_q";
const TRUNK_TARGET: &str = "9138000138000";

const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_answered",
    "call_hangup",
    "queue_agent_connected",
    "call_transferred",
];

fn queue_strategy(targets: Vec<RouteQueueTargetConfig>) -> RouteQueueStrategyConfig {
    RouteQueueStrategyConfig {
        mode: QueueDialMode::Sequential,
        wait_timeout_secs: Some(30),
        targets,
    }
}

fn proxy_config(trunk_dest_port: u16) -> ProxyConfig {
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
    // The transfer target resolves through the route table (queue hand-off /
    // trunk forward) — same switch the SIP REFER path gates on.
    config.route_originated_calls = true;

    // Source queue: customer → skill group (bob answers).
    config.queues.insert(
        SOURCE_QUEUE.to_string(),
        RouteQueueConfig {
            name: Some(SOURCE_QUEUE.to_string()),
            strategy: queue_strategy(vec![RouteQueueTargetConfig {
                uri: format!("skill-group:{SKILL_GROUP}"),
                label: None,
            }]),
            accept_immediately: false,
            ..Default::default()
        },
    );

    // Transfer-target queue: OFFLINE member — the hand-off must start the
    // QueueApp in-session without any INVITE leaving for "8000".
    config.queues.insert(
        ROUTED_QUEUE_NAME.to_string(),
        RouteQueueConfig {
            name: Some(ROUTED_QUEUE_NAME.to_string()),
            strategy: queue_strategy(vec![RouteQueueTargetConfig {
                uri: "sip:offline-agent@10.255.255.1".to_string(),
                label: None,
            }]),
            accept_immediately: false,
            ..Default::default()
        },
    );

    config.routes = Some(vec![
        RouteRule {
            name: "route_to_source_queue".to_string(),
            priority: 10,
            match_conditions: MatchConditions {
                to_user: Some(QUEUE_NUMBER.to_string()),
                ..Default::default()
            },
            action: RouteAction {
                action: Some("queue".to_string()),
                queue: Some(SOURCE_QUEUE.to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        RouteRule {
            name: "route_to_target_queue".to_string(),
            priority: 20,
            match_conditions: MatchConditions {
                to_user: Some(ROUTED_QUEUE_NUMBER.to_string()),
                ..Default::default()
            },
            action: RouteAction {
                action: Some("queue".to_string()),
                queue: Some(ROUTED_QUEUE_NAME.to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
        RouteRule {
            name: "route_trunk_prefix".to_string(),
            priority: 30,
            match_conditions: MatchConditions {
                request_uri_user: Some("91.*".to_string()),
                ..Default::default()
            },
            action: RouteAction {
                dest: Some(DestConfig::Single("gw1".to_string())),
                select: "rr".to_string(),
                ..Default::default()
            },
            ..Default::default()
        },
    ]);

    let mut trunks = std::collections::HashMap::new();
    trunks.insert(
        "gw1".to_string(),
        TrunkConfig {
            dest: format!("sip:127.0.0.1:{trunk_dest_port}"),
            ..Default::default()
        },
    );
    config.trunks = trunks;
    config
}

struct Harness {
    server: E2eTestServer,
    capture: Arc<WebhookCapture>,
}

/// Server + CC registry + CC session hook (production wiring) with the
/// source queue, the routed target queue, and the trunk route armed.
async fn start_harness(trunk_dest_port: u16) -> Result<Harness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Blind Transfer Q".to_string()),
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

    let capture = Arc::new(WebhookCapture::start().await);
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

    let mut proxy_config = proxy_config(trunk_dest_port);
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;

    // Passwords must match `E2eTestServer::create_ua`'s per-user table.
    let passwords = [("bob", "password456"), ("caller", "password"), ("alice", "password123"), ("trunkgw", "password")];
    let users = passwords
        .into_iter()
        .enumerate()
        .map(|(i, (name, password))| SipUser {
            id: i as u64 + 1,
            username: name.to_string(),
            password: Some(password.to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        })
        .collect();

    // The hook's call registry is late-bound (mirrors CcAddon startup).
    let call_registry_container: Arc<
        tokio::sync::RwLock<Option<Arc<ActiveProxyCallRegistry>>>,
    > = Arc::new(tokio::sync::RwLock::new(None));
    let session_hook = CcCallSessionHook::new(cc_registry.clone(), Arc::new(MetricsCollector::new()))
        .with_cc_config(Arc::new(CcConfig::default()))
        .with_call_registry_shared(call_registry_container.clone());

    let server = E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            queue_enricher: None,
            users,
            session_hook: Some(Arc::new(session_hook)),
            agent_registry: Some(adapter),
            rwi_gateway: Some(gateway),
            #[cfg(feature = "addon-cc")]
            cc_policy_db: None,
        },
    )
    .await?;
    *call_registry_container.write().await = Some(server.registry.clone());

    Ok(Harness { server, capture })
}

/// UA loop that answers the FIRST incoming call and stays on it.
async fn answer_first_incoming(ua: &TestUa, sdp: &str, label: &str) -> Result<DialogId> {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "{label}: no incoming call arrived");
        for event in ua.process_dialog_events().await.unwrap_or_default() {
            if let TestUaEvent::IncomingCall(id, _) = event {
                let _ = ua.ring_call(&id).await;
                sleep(Duration::from_millis(100)).await;
                ua.answer_call(&id, Some(sdp.to_string())).await?;
                return Ok(id);
            }
        }
        sleep(Duration::from_millis(30)).await;
    }
}

async fn wait_terminated(
    ua: &TestUa,
    call_id: &str,
    window: Duration,
    label: &str,
) -> Result<()> {
    wait_specific_terminated(ua, call_id, window, label).await
}

async fn wait_specific_terminated(
    ua: &TestUa,
    call_id: &str,
    window: Duration,
    label: &str,
) -> Result<()> {
    let deadline = Instant::now() + window;
    loop {
        anyhow::ensure!(Instant::now() < deadline, "{label}");
        for event in ua.process_dialog_events().await.unwrap_or_default() {
            if let TestUaEvent::CallTerminated(id) = event {
                if id.call_id == call_id {
                    return Ok(());
                }
            }
        }
        sleep(Duration::from_millis(30)).await;
    }
}

async fn assert_stays_up(
    ua: &TestUa,
    call_id: &str,
    window: Duration,
    label: &str,
) -> Result<()> {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        for event in ua.process_dialog_events().await.unwrap_or_default() {
            anyhow::ensure!(
                !matches!(event, TestUaEvent::CallTerminated(ref id) if id.call_id == call_id),
                "{label}"
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn wait_transferred(capture: &WebhookCapture, window: Duration) -> Result<serde_json::Value> {
    let deadline = Instant::now() + window;
    loop {
        anyhow::ensure!(Instant::now() < deadline, "call_transferred never arrived");
        {
            let received = capture.received.lock().unwrap();
            for body in received.iter() {
                if body["event_type"] == "call_transferred" {
                    return Ok(body.clone());
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Customer → queue → bob answers; returns the customer dialog, the agent
/// dialog, and the owning session handle (CTI command entry point).
async fn established_queue_call(
    server: &E2eTestServer,
    caller: &TestUa,
    bob: &TestUa,
    sdp: &str,
) -> Result<(DialogId, DialogId, rustpbx::proxy::proxy_call::sip_session::SipSessionHandle)> {
    let caller_dialog = caller.make_call(QUEUE_NUMBER, Some(sdp.to_string())).await?;
    let bob_dialog = answer_first_incoming(bob, sdp, "queue agent").await?;
    // Agent attribution pins at answer; give the connected hooks a beat.
    sleep(Duration::from_millis(600)).await;
    let handle = server
        .registry
        .get_handle_by_call_id(&caller_dialog.call_id)
        .ok_or_else(|| anyhow!("customer call-id must resolve to its owner session"))?;
    Ok((caller_dialog, bob_dialog, handle))
}

fn transfer_command(target: &str) -> CallCommand {
    CallCommand::BlindTransferCustomer {
        target: target.to_string(),
        headers: Default::default(),
        application_only: false,
        completion: None,
    }
}

/// Case 1 — registered extension: the INVITE reaches alice on an independent
/// dialog, the agent is BYE'd at dispatch, and once alice answers BOTH the
/// customer and the target stay up (the stale-callee teardown regression the
/// ignored raw-command test in cc_e2e used to trip on). Alice hanging up
/// cascades BYE to the surviving customer dialog.
#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_blind_transfer_customer_registered_extension() {
    let _ = tracing_subscriber::fmt().try_init();
    let harness = start_harness(portpicker::pick_unused_port().unwrap_or(29111))
        .await
        .unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12345, false);
    let caller = harness.server.create_ua("caller").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();
    let target = harness.server.create_ua("alice").await.unwrap();

    let (caller_dialog, bob_dialog, handle) =
        established_queue_call(&harness.server, &caller, &bob, &sdp)
            .await
            .unwrap();

    handle
        .send_command(transfer_command("alice"))
        .expect("command dispatch");

    let target_dialog = answer_first_incoming(&target, &sdp, "transfer target")
        .await
        .unwrap();
    wait_terminated(
        &bob,
        &bob_dialog.call_id,
        Duration::from_secs(3),
        "agent must be released at hand-off dispatch",
    )
    .await
    .unwrap();

    sleep(Duration::from_millis(700)).await;
    assert_stays_up(
        &caller,
        &caller_dialog.call_id,
        Duration::from_millis(700),
        "customer must survive the transfer",
    )
    .await
    .unwrap();
    assert_stays_up(
        &target,
        &target_dialog.call_id,
        Duration::from_millis(400),
        "target must not be torn down right after answering (stale-callee guard)",
    )
    .await
    .unwrap();

    let transferred = wait_transferred(&harness.capture, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(transferred["event"]["transfer_target_type"], "sip");
    assert!(transferred["event"]["transfer_target"]
        .as_str()
        .is_some_and(|t| t.contains("alice")));
    // Queue-served call transferred out: the origin names the serving queue.
    assert_eq!(
        transferred["event"]["transfer_source"]["source_type"], "queue",
        "transfer origin: {}",
        transferred["event"]
    );
    assert_eq!(
        transferred["event"]["transfer_source"]["name"], SOURCE_QUEUE,
        "transfer origin queue: {}",
        transferred["event"]
    );

    target.hangup(&target_dialog).await.unwrap();
    wait_specific_terminated(
        &caller,
        &caller_dialog.call_id,
        Duration::from_secs(3),
        "target hangup must cascade BYE to the surviving customer dialog",
    )
    .await
    .unwrap();

    caller.stop();
    bob.stop();
    target.stop();
    harness.server.stop();
}

/// Case 2 — bare number routed to a QUEUE: the hand-off starts the QueueApp
/// IN-SESSION (agent released, `call_transferred` kind=queue, customer stays
/// on the same session) and must NOT dumb-dial "8000" — the old behavior
/// re-entered the route as a second inbound call.
#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_blind_transfer_customer_routed_queue() {
    let _ = tracing_subscriber::fmt().try_init();
    let harness = start_harness(portpicker::pick_unused_port().unwrap_or(29112))
        .await
        .unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12345, false);
    let caller = harness.server.create_ua("caller").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();

    let (caller_dialog, bob_dialog, handle) =
        established_queue_call(&harness.server, &caller, &bob, &sdp)
            .await
            .unwrap();

    handle
        .send_command(transfer_command(ROUTED_QUEUE_NUMBER))
        .expect("command dispatch");

    wait_terminated(
        &bob,
        &bob_dialog.call_id,
        Duration::from_secs(3),
        "agent must be released with the in-session queue hand-off",
    )
    .await
    .unwrap();

    let transferred = wait_transferred(&harness.capture, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(transferred["event"]["transfer_target_type"], "queue");
    assert!(transferred["event"]["transfer_target"]
        .as_str()
        .is_some_and(|t| t.contains(ROUTED_QUEUE_NUMBER)));
    // Queue-served call handed off in-session: the origin names the serving
    // (source) queue, not the routed destination.
    assert_eq!(
        transferred["event"]["transfer_source"]["source_type"], "queue",
        "transfer origin: {}",
        transferred["event"]
    );
    assert_eq!(
        transferred["event"]["transfer_source"]["name"], SOURCE_QUEUE,
        "transfer origin queue: {}",
        transferred["event"]
    );

    // The customer remains on the SAME session (QueueApp serves them); the
    // offline member dial fails in the background without ending the call.
    assert_stays_up(
        &caller,
        &caller_dialog.call_id,
        Duration::from_millis(1500),
        "customer must stay on the queue app after the hand-off",
    )
    .await
    .unwrap();
    assert_eq!(
        harness.server.get_active_calls().len(),
        1,
        "in-session hand-off must not spawn a second session (no dumb-dial of {ROUTED_QUEUE_NUMBER})"
    );

    caller.hangup(&caller_dialog).await.unwrap();
    caller.stop();
    bob.stop();
    harness.server.stop();
}

/// Case 3 — bare number routed FORWARD to a trunk: the INVITE lands on the
/// trunk destination (route match + trunk selection), the event stays sip,
/// and the surviving dialogs behave like Case 1.
#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_blind_transfer_customer_trunk_forward() {
    let _ = tracing_subscriber::fmt().try_init();
    let trunk_port = portpicker::pick_unused_port().unwrap_or(29222);
    let harness = start_harness(trunk_port).await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12345, false);
    let caller = harness.server.create_ua("caller").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();
    // The trunk peer binds the exact port the gw1 trunk destination points at
    // (unregistered — the INVITE is addressed to the trunk destination).
    let mut trunk = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "trunkgw".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: trunk_port,
        proxy_addr: harness.server.proxy_addr,
    });
    trunk.start().await.expect("trunk UA start");

    let (caller_dialog, bob_dialog, handle) =
        established_queue_call(&harness.server, &caller, &bob, &sdp)
            .await
            .unwrap();

    handle
        .send_command(transfer_command(TRUNK_TARGET))
        .expect("command dispatch");

    let trunk_dialog = answer_first_incoming(&trunk, &sdp, "trunk peer")
        .await
        .unwrap();
    wait_terminated(
        &bob,
        &bob_dialog.call_id,
        Duration::from_secs(3),
        "agent must be released at hand-off dispatch",
    )
    .await
    .unwrap();

    sleep(Duration::from_millis(700)).await;
    assert_stays_up(
        &caller,
        &caller_dialog.call_id,
        Duration::from_millis(700),
        "customer must survive the trunk transfer",
    )
    .await
    .unwrap();

    let transferred = wait_transferred(&harness.capture, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(transferred["event"]["transfer_target_type"], "sip");
    assert!(transferred["event"]["transfer_target"]
        .as_str()
        .is_some_and(|t| t.contains(TRUNK_TARGET)));
    // Queue-served call forwarded to a trunk: the origin still names the
    // serving queue.
    assert_eq!(
        transferred["event"]["transfer_source"]["source_type"], "queue",
        "transfer origin: {}",
        transferred["event"]
    );
    assert_eq!(
        transferred["event"]["transfer_source"]["name"], SOURCE_QUEUE,
        "transfer origin queue: {}",
        transferred["event"]
    );

    trunk.hangup(&trunk_dialog).await.unwrap();
    wait_specific_terminated(
        &caller,
        &caller_dialog.call_id,
        Duration::from_secs(3),
        "trunk hangup must cascade BYE to the surviving customer dialog",
    )
    .await
    .unwrap();

    caller.stop();
    bob.stop();
    trunk.stop();
    harness.server.stop();
}
