//! SIP e2e: agent status tracking for calls that reach an agent in ACW
//! (after-call work / `Wrapup`).
//!
//! Regression scenario (user-reported): an agent in ACW / on a break
//! receives a blind transfer / CTI / direct dial. The call is delivered to
//! the softphone and answered, but the server status machine used to ignore
//! it entirely (no Ringing, no Busy, no capacity) — so the PREVIOUS call's
//! wrapup timer later flipped a MID-CALL agent to `Idle` while the client
//! still showed the call (server/client desync + ACD re-dispatching into a
//! live phone).
//!
//! Covered deliveries:
//! 1. direct internal dial (extension → extension) during ACW;
//! 2. CTI blind transfer (`CallCommand::BlindTransferCustomer`) whose target
//!    agent is in ACW.
//!
//! The short `wrapup_time_secs` (4s) on the skill group arms the previous
//! call's wrapup timer INSIDE the next call's talk time — the core assertion
//! is that the timer fires without flipping the mid-call agent to `Idle`.

use anyhow::{Result, anyhow};
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent::AgentStatus;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::cc_call_session_hook::CcCallSessionHook;
use rustpbx::addons::cc::config::CcConfig;
use rustpbx::addons::cc::metrics::MetricsCollector;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::domain::{CallCommand, LegId, LegPurpose};
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use rsipstack::dialog::DialogId;
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaEvent, create_test_sdp, create_test_sdp_answer};

const QUEUE_NUMBER: &str = "9400";
const QUEUE_NAME: &str = "acw_src_q";
const SKILL_GROUP: &str = "sg_acw_status";
/// Short enough that the FIRST call's wrapup timer expires INSIDE the next
/// call's talk time — that collision is the regression being guarded.
const WRAPUP_SECS: u64 = 4;
/// Strictly longer than `WRAPUP_SECS`: the mid-call observation window must
/// straddle the timer expiry.
const TIMER_OVERSHOOT_SECS: u64 = WRAPUP_SECS + 2;

struct Harness {
    server: E2eTestServer,
    cc_registry: Arc<CcAgentRegistry>,
}

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
    // The CTI transfer target leg must resolve through the route table.
    config.route_originated_calls = true;

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

    config.routes = Some(vec![RouteRule {
        name: "route_to_acw_queue".to_string(),
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

async fn start_harness(agents: &[&str]) -> Result<Harness> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None).await.unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("ACW Status Q".to_string()),
            skills_required: vec!["support".to_string()],
            overflow_groups: vec![],
            sla_target_secs: 30,
            max_wait_secs: 120,
            metadata: Some(serde_json::json!({ "wrapup_time_secs": WRAPUP_SECS })),
        },
    )
    .await
    .unwrap();

    let cc_registry = Arc::new(CcAgentRegistry::with_db(db.clone()));
    for agent in agents {
        cc_registry
            .register((*agent).to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status(agent, AgentStatus::Idle)
            .await
            .unwrap();
    }

    let adapter = Arc::new(CcAgentRegistryAdapter::new(
        cc_registry.clone(),
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: false,
            ..AcdConfig::default()
        })),
        "localhost",
    ));

    // The hook's call registry is late-bound (mirrors CcAddon startup).
    let call_registry_container: Arc<
        tokio::sync::RwLock<Option<Arc<ActiveProxyCallRegistry>>>,
    > = Arc::new(tokio::sync::RwLock::new(None));
    let session_hook = CcCallSessionHook::new(cc_registry.clone(), Arc::new(MetricsCollector::new()))
        .with_cc_config(Arc::new(CcConfig::default()))
        .with_call_registry_shared(call_registry_container.clone());

    let passwords = [
        ("bob", "password456"),
        ("alice", "password123"),
        ("caller", "password"),
        ("caller2", "password"),
    ];
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

    // RWI gateway (production wiring): the transfer flow emits
    // `call_transferred` through it; run it with a black-hole capture so the
    // shape matches CcAddon startup.
    let gateway: rustpbx::rwi::RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = rustpbx::rwi::RwiGateway::new();
        gw.set_webhook_tx(rustpbx::rwi::webhook::start_rwi_webhook_handler(
            rustpbx::config::LocatorWebhookConfig {
                url: "http://127.0.0.1:9".to_string(),
                events: vec![],
                headers: None,
                timeout_ms: Some(1000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let server = E2eTestServer::start_with_inject(
        proxy_config(),
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

    Ok(Harness {
        server,
        cc_registry,
    })
}

/// Agent UA loop that RINGS every incoming call and holds it there until
/// `answer_now` is raised — lets the test observe the RINGING-phase server
/// status deterministically before the answer lands.
///
/// Single-writer protocol: ONLY the test writes the flag (the pump never
/// resets it — a reset here used to wipe the test's release that raced the
/// ring, deadlocking the pump).
fn spawn_gated_pump(ua: TestUa, answer_now: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        if let TestUaEvent::IncomingCall(id, offer) = ev {
                            let _ = ua.ring_call(&id).await;
                            while !answer_now.load(Ordering::SeqCst) {
                                sleep(Duration::from_millis(20)).await;
                            }
                            let sdp = offer
                                .as_deref()
                                .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0));
                            if let Err(e) = ua.answer_call(&id, sdp).await {
                                eprintln!("[gated-pump] answer_call failed: {e}");
                            }
                        }
                    }
                }
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
}

/// Customer UA loop that answers the first incoming call (the queue's agent
/// leg / transfer target leg) and stays on it.
fn spawn_auto_answer_pump(ua: TestUa) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        if let TestUaEvent::IncomingCall(id, offer) = ev {
                            let _ = ua.ring_call(&id).await;
                            sleep(Duration::from_millis(80)).await;
                            let sdp = offer
                                .as_deref()
                                .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0));
                            let _ = ua.answer_call(&id, sdp).await;
                        }
                    }
                }
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
}

/// Poll the CC registry until `pred` holds; returns the LAST observed status
/// on timeout so the assert message shows the actual stuck state.
async fn wait_status(
    registry: &Arc<CcAgentRegistry>,
    agent: &str,
    window: Duration,
    label: &str,
    pred: impl Fn(&AgentStatus) -> bool,
) -> AgentStatus {
    let deadline = Instant::now() + window;
    let mut last = registry
        .get_agent(agent)
        .await
        .map(|a| a.status)
        .unwrap_or(AgentStatus::Offline);
    loop {
        if pred(&last) {
            return last;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: last status = {}",
            last
        );
        sleep(Duration::from_millis(50)).await;
        if let Some(a) = registry.get_agent(agent).await {
            last = a.status;
        }
    }
}

/// caller → queue → `agent` rings (observed) → answers (released via
/// `gate`) → Busy. Returns the caller dialog.
async fn established_queue_call(
    _server: &E2eTestServer,
    registry: &Arc<CcAgentRegistry>,
    caller: &TestUa,
    agent: &str,
    sdp: &str,
    gate: &Arc<AtomicBool>,
) -> Result<DialogId> {
    // Pre-release the gate: call1's answer needs no mid-ring observation (the
    // queue ring may be answered before a 50ms poll catches `Ringing`).
    gate.store(true, Ordering::SeqCst);
    let caller_dialog = caller.make_call(QUEUE_NUMBER, Some(sdp.to_string())).await?;
    // Either phase is observable: an instant answer skips past `Ringing`.
    wait_status(
        registry,
        agent,
        Duration::from_secs(8),
        "queue leg must reach Ringing or Busy",
        |s| matches!(s, AgentStatus::Ringing { .. } | AgentStatus::Busy { .. }),
    )
    .await;
    wait_status(
        registry,
        agent,
        Duration::from_secs(8),
        "answered queue call must reach Busy",
        |s| matches!(s, AgentStatus::Busy { .. }),
    )
    .await;
    Ok(caller_dialog)
}

// -------------------------------------------------------------------------
// Case 1 — direct internal dial (extension → extension) during ACW.
// -------------------------------------------------------------------------

#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_direct_call_during_acw_is_tracked_and_wrapup_timer_cannot_idle_mid_call() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    // Only bob is a dispatchable CC agent — the direct-dial peer alice is a
    // plain SIP user, so the queue dispatch is deterministic.
    let harness = start_harness(&["bob"]).await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12345, false);

    let caller = harness.server.create_ua("caller").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();
    let alice = harness.server.create_ua("alice").await.unwrap();

    let bob_gate = Arc::new(AtomicBool::new(false));
    let _bob_pump = spawn_gated_pump(bob.clone(), bob_gate.clone());
    let _alice_pump = spawn_auto_answer_pump(alice.clone());
    sleep(Duration::from_millis(300)).await;

    // 1. Queue call → bob answers → talk.
    let caller_dialog = established_queue_call(
        &harness.server,
        &harness.cc_registry,
        &caller,
        "bob",
        &sdp,
        &bob_gate,
    )
    .await
    .unwrap();
    wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "bob must be Busy after answering the queue call",
        |s| matches!(s, AgentStatus::Busy { .. }),
    )
    .await;

    // 2. Caller hangs up → bob lands in ACW with the 4s wrapup timer armed.
    caller.hangup(&caller_dialog).await.unwrap();
    let wrapup_status = wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "bob must enter Wrapup when the queue call ends",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;
    let acw_call_id = wrapup_status
        .current_call_id()
        .ok_or_else(|| anyhow!("Wrapup must be bound to the ended call"))?
        .to_string();

    // 3. DIRECT dial alice → bob while bob is in ACW. The gated pump holds
    // the ring so the RINGING-phase status is observed deterministically —
    // make_call must be awaited CONCURRENTLY (it blocks until the answer).
    bob_gate.store(false, Ordering::SeqCst);
    let alice_dial = {
        let ua = alice.clone();
        let sdp_offer = sdp.clone();
        tokio::spawn(async move { ua.make_call("bob", Some(sdp_offer)).await })
    };
    let ringing = wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "ring during ACW must transition Wrapup → Ringing (server must not hide the call)",
        |s| matches!(s, AgentStatus::Ringing { .. }),
    )
    .await;
    let direct_call_id = ringing
        .current_call_id()
        .ok_or_else(|| anyhow!("Ringing must be bound to the direct call"))?
        .to_string();
    assert_ne!(
        direct_call_id, acw_call_id,
        "the ring must be bound to the NEW call, not the ACW call"
    );

    // 4. Bob answers → tracked Busy with capacity.
    bob_gate.store(true, Ordering::SeqCst);
    let direct_dialog = tokio::time::timeout(Duration::from_secs(10), alice_dial)
        .await
        .map_err(|_| anyhow!("alice's make_call did not settle"))?
        .map_err(|e| anyhow!("alice's make_call task failed: {e}"))?
        .map_err(|e| anyhow!("alice's make_call failed: {e}"))?;
    wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "answered direct call must transition to Busy",
        |s| matches!(s, AgentStatus::Busy { call_id, .. } if *call_id == direct_call_id),
    )
    .await;
    let agent = harness.cc_registry.get_agent("bob").await.unwrap();
    assert_eq!(
        agent.current_calls, 1,
        "the tracked direct call must consume agent capacity"
    );

    // 5. THE REGRESSION: the previous call's wrapup timer expires mid-call —
    // it must NOT flip the talking agent to Idle.
    sleep(Duration::from_secs(TIMER_OVERSHOOT_SECS)).await;
    let agent = harness.cc_registry.get_agent("bob").await.unwrap();
    assert!(
        matches!(&agent.status, AgentStatus::Busy { call_id, .. } if *call_id == direct_call_id),
        "expired ACW timer must not Idle a mid-call agent, got {}",
        agent.status
    );
    assert_eq!(agent.current_calls, 1);

    // 6. Direct call ends → bob enters ITS OWN ACW (stays there — the direct
    // call has no skill group, so the default long wrapup applies).
    alice.hangup(&direct_dialog).await.unwrap();
    let after = wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "ended direct call must enter its own Wrapup",
        |s| matches!(s, AgentStatus::Wrapup { call_id, .. } if *call_id == direct_call_id),
    )
    .await;
    assert_eq!(
        after.current_call_id().as_deref(),
        Some(direct_call_id.as_str()),
        "the new ACW must be bound to the direct call"
    );
    let agent = harness.cc_registry.get_agent("bob").await.unwrap();
    assert_eq!(agent.current_calls, 0);
    Ok(())
}

// -------------------------------------------------------------------------
// Case 2 — CTI blind transfer whose TARGET agent is in ACW.
//
// The transfer leg carries `purpose=Agent` (UUI leg=agent) + the target's
// digest credentials, so the agent-target dial path differs from the plain
// extension path.
//
// Historical note: this case was `#[ignore]`d for months because
// `BlindTransferCustomer` released the transferring agent AT DIAL TIME and
// the dispatch-time BYE cancelled the transfer leg ~2ms after the INVITE
// (487 Request Terminated) before the target could ring — deterministically,
// on every agent target. The release now rides the target leg's answer edge
// (`PendingCustomerTransferRelease`), so the ignore is lifted and this body
// asserts the full tracking chain (ACW target → transfer → Busy{new call} →
// the previous call's wrapup timer cannot Idle mid-call → own Wrapup).
// -------------------------------------------------------------------------


#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_blind_transfer_target_in_acw_is_refused_not_rung() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    // Only bob starts as a CC agent (deterministic call1 dispatch). alice is
    // registered into the CC registry AFTER call1: by then bob sits in ACW
    // (not dispatchable), so call2 deterministically lands on alice.
    let harness = start_harness(&["bob"]).await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12346, false);

    let caller1 = harness.server.create_ua("caller").await.unwrap();
    let caller2 = harness.server.create_ua("caller2").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();
    let alice = harness.server.create_ua("alice").await.unwrap();

    let bob_gate = Arc::new(AtomicBool::new(false));
    let _bob_pump = spawn_gated_pump(bob.clone(), bob_gate.clone());
    let _alice_pump = spawn_auto_answer_pump(alice.clone());
    sleep(Duration::from_millis(300)).await;

    // 1. call1: caller1 → queue → bob; hangup → bob in ACW (4s timer).
    let caller1_dialog = established_queue_call(
        &harness.server,
        &harness.cc_registry,
        &caller1,
        "bob",
        &sdp,
        &bob_gate,
    )
    .await
    .unwrap();
    caller1.hangup(&caller1_dialog).await.unwrap();
    wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "bob must enter Wrapup after call1",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;

    // alice joins the CC registry now: bob (ACW) is not dispatchable, so
    // call2 must dispatch to alice.
    harness
        .cc_registry
        .register("alice".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    harness
        .cc_registry
        .update_status("alice", AgentStatus::Idle)
        .await
        .unwrap();

    // 2. call2: caller2 → queue → alice answers (bob is in ACW, not
    // dispatchable — capacity must stay with alice).
    let caller2_dialog = caller2
        .make_call(QUEUE_NUMBER, Some(sdp.clone()))
        .await
        .unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must be dispatched and Busy on call2",
        |s| matches!(s, AgentStatus::Busy { .. }),
    )
    .await;

    // 3. CTI blind transfer of the customer to bob while he is in ACW: the
    // transfer dial is REFUSED before any ring — the ACW target must not be
    // rung (dial-time exclusion, production 2026-11-16).
    let handle = harness
        .server
        .registry
        .get_handle_by_call_id(&caller2_dialog.call_id)
        .ok_or_else(|| anyhow!("call2 must resolve to its owner session"))?;
    handle
        .send_command(CallCommand::BlindTransferCustomer {
            target: "bob".to_string(),
            headers: Default::default(),
            application_only: false,
            operator_agent: None,
            operator_leg_hint: None,
            completion: None,
        })
        .expect("transfer command dispatch");

    // 4. bob stays Wrapup (never rung, never Busy) and alice's leg survives.
    // A short bounded wait that must NOT observe the ACW target leaving
    // Wrapup (no ring, no Busy, no re-queue admission).
    sleep(Duration::from_secs(2)).await;
    let bob_still = harness.cc_registry.get_agent("bob").await.unwrap();
    assert!(
        matches!(bob_still.status, AgentStatus::Wrapup { .. }),
        "the ACW target must stay Wrapup (never rung), got {}",
        bob_still.status
    );
    let bob_after = harness.cc_registry.get_agent("bob").await.unwrap();
    assert!(
        matches!(bob_after.status, AgentStatus::Wrapup { .. }),
        "the ACW target must stay in Wrapup, got {}",
        bob_after.status
    );
    assert_eq!(bob_after.current_calls, 0, "the ACW target was not rung");

    // 5. The customer↔alice call survives the refused transfer: the customer
    // hangs up → the call ends cleanly.
    caller2.hangup(&caller2_dialog).await.unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must enter her own Wrapup after the hangup",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;
    Ok(())
}

// -------------------------------------------------------------------------
// Multi-step transfer, STEP 1 (CTI consult): the consult dial itself is
// refused when the target sits in ACW/Wrapup — the helpdesk agent must
// never hear the consult ring (dial-time exclusion, production 2026-11-16: 1001
// consulted 1002 while 1002 was in wrapup; the wrapup agent got rung).
// The LegAdd failure surfaces to the consulting agent via the CTI API;
// the customer↔consulting-agent call is untouched.
// -------------------------------------------------------------------------

#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_consult_target_in_acw_is_refused_not_rung() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    // Identical deterministic shape to the blind-transfer case: bob takes
    // call1 and lands in ACW; alice joins afterwards and owns call2.
    let harness = start_harness(&["bob"]).await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12346, false);

    let caller1 = harness.server.create_ua("caller").await.unwrap();
    let caller2 = harness.server.create_ua("caller2").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();
    let alice = harness.server.create_ua("alice").await.unwrap();

    let bob_gate = Arc::new(AtomicBool::new(false));
    let _bob_pump = spawn_gated_pump(bob.clone(), bob_gate.clone());
    let _alice_pump = spawn_auto_answer_pump(alice.clone());
    sleep(Duration::from_millis(300)).await;

    // 1. call1: caller1 → queue → bob; hangup → bob in ACW (4s timer).
    let caller1_dialog = established_queue_call(
        &harness.server,
        &harness.cc_registry,
        &caller1,
        "bob",
        &sdp,
        &bob_gate,
    )
    .await
    .unwrap();
    caller1.hangup(&caller1_dialog).await.unwrap();
    wait_status(
        &harness.cc_registry,
        "bob",
        Duration::from_secs(5),
        "bob must enter Wrapup after call1",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;

    // alice joins the CC registry now: bob (ACW) is not dispatchable, so
    // call2 must dispatch to alice.
    harness
        .cc_registry
        .register("alice".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    harness
        .cc_registry
        .update_status("alice", AgentStatus::Idle)
        .await
        .unwrap();

    // 2. call2: caller2 → queue → alice answers.
    let caller2_dialog = caller2
        .make_call(QUEUE_NUMBER, Some(sdp.clone()))
        .await
        .unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must be dispatched and Busy on call2",
        |s| matches!(s, AgentStatus::Busy { .. }),
    )
    .await;

    // 3. STEP 1 of a multi-step transfer: alice consults bob (ACW) via the
    // CTI LegAdd consult dial. The command channel is fire-and-forget; the
    // outcome is asserted through the session snapshot and the CC registry.
    let handle = harness
        .server
        .registry
        .get_handle_by_call_id(&caller2_dialog.call_id)
        .ok_or_else(|| anyhow!("call2 must resolve to its owner session"))?;
    handle
        .send_command(CallCommand::LegAdd {
            source_leg: None,
            target: "bob".to_string(),
            leg_id: Some(LegId::from("consult")),
            headers: Vec::new(),
            purpose: Some(LegPurpose::Consult),
        })
        .expect("consult command dispatch");

    // 4. The gate must refuse BEFORE any leg is created or INVITE sent:
    //    the session keeps its two legs (customer + alice) — a rung consult
    //    would add a third "consult" leg — and bob is never rung.
    sleep(Duration::from_secs(3)).await;
    let snapshot = handle
        .snapshot()
        .ok_or_else(|| anyhow!("call2 must publish a session snapshot"))?;
    assert_eq!(
        snapshot.leg_count, 2,
        "the refused consult must not create a consult leg (caller + agent only)"
    );
    let bob_after = harness.cc_registry.get_agent("bob").await.unwrap();
    assert!(
        matches!(bob_after.status, AgentStatus::Wrapup { .. }),
        "the ACW target must stay Wrapup (consult never rang), got {}",
        bob_after.status
    );
    assert_eq!(bob_after.current_calls, 0, "the ACW target was not rung");
    let alice_after = harness.cc_registry.get_agent("alice").await.unwrap();
    assert!(
        matches!(alice_after.status, AgentStatus::Busy { .. }),
        "the customer↔alice call must survive the refused consult, got {}",
        alice_after.status
    );

    // 5. The customer hangup ends the call cleanly (alice → her own Wrapup).
    caller2.hangup(&caller2_dialog).await.unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must enter her own Wrapup after the hangup",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;
    Ok(())
}

// -------------------------------------------------------------------------
// Away (break) at the wiring level: the ACW tests above prove the Wrapup
// branch end-to-end; this proves the Away branch rides the same gate -
// the queue dispatch skips the break agent AND a blind transfer to them
// is refused before any ring (dial-time exclusion, production
// 2026-11-16: consulted agents must not be dialed).
// -------------------------------------------------------------------------
#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_blind_transfer_target_on_break_is_refused_not_rung() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let harness = start_harness(&["bob"]).await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12446, false);

    // bob starts on break: not dispatchable, and not a legal transfer target.
    harness
        .cc_registry
        .update_status("bob", AgentStatus::Away("2".to_string()))
        .await
        .unwrap();

    let caller = harness.server.create_ua("caller").await.unwrap();
    let bob = harness.server.create_ua("bob").await.unwrap();
    let alice = harness.server.create_ua("alice").await.unwrap();
    // bob's pump WOULD answer if the exclusion broke - a ring is detectable.
    let bob_gate = Arc::new(AtomicBool::new(false));
    let _bob_pump = spawn_gated_pump(bob.clone(), bob_gate.clone());
    let _alice_pump = spawn_auto_answer_pump(alice.clone());
    sleep(Duration::from_millis(300)).await;

    // alice joins the registry now: with bob on break, the queue dispatch
    // must land on alice alone (the break agent is excluded from dispatch).
    harness
        .cc_registry
        .register("alice".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    harness
        .cc_registry
        .update_status("alice", AgentStatus::Idle)
        .await
        .unwrap();

    let caller_dialog = caller
        .make_call(QUEUE_NUMBER, Some(sdp.clone()))
        .await
        .unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must be dispatched (bob on break is excluded)",
        |s| matches!(s, AgentStatus::Busy { .. }),
    )
    .await;
    let bob_during = harness.cc_registry.get_agent("bob").await.unwrap();
    assert!(
        matches!(bob_during.status, AgentStatus::Away(_)),
        "the break agent must stay Away during dispatch, got {}",
        bob_during.status
    );

    // Blind transfer to the break target: refused before any ring.
    let handle = harness
        .server
        .registry
        .get_handle_by_call_id(&caller_dialog.call_id)
        .ok_or_else(|| anyhow!("call must resolve to its owner session"))?;
    handle
        .send_command(CallCommand::BlindTransferCustomer {
            target: "bob".to_string(),
            headers: Default::default(),
            application_only: false,
            operator_agent: None,
            operator_leg_hint: None,
            completion: None,
        })
        .expect("transfer command dispatch");

    sleep(Duration::from_secs(2)).await;
    let bob_after = harness.cc_registry.get_agent("bob").await.unwrap();
    assert!(
        matches!(bob_after.status, AgentStatus::Away(_)),
        "the break target must stay Away (never rung), got {}",
        bob_after.status
    );
    assert_eq!(bob_after.current_calls, 0, "the break target was not rung");

    // The customer↔alice call survives the refused transfer.
    caller.hangup(&caller_dialog).await.unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must enter her own Wrapup after the hangup",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;
    Ok(())
}

// -------------------------------------------------------------------------
// Self-dispatch exclusion at the wiring level (production 2026-11-15: 1003
// dialed *81support and ACD handed the call BACK to 1003 - the customer
// talked to themselves). A registered idle agent dialing the queue from
// their own device must never be dispatched to themselves: the other agent
// takes the call and the caller stays Idle throughout.
// -------------------------------------------------------------------------
#[tokio::test]
#[cfg(feature = "addon-cc")]
async fn e2e_queue_dispatch_never_hands_the_call_back_to_the_caller_agent() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    // Both bob and alice are registered idle agents; bob's own device is
    // the queue caller.
    let harness = start_harness(&["bob", "alice"]).await.unwrap();
    let sdp = create_test_sdp("127.0.0.1", 12546, false);

    let bob = harness.server.create_ua("bob").await.unwrap();
    let alice = harness.server.create_ua("alice").await.unwrap();
    let _alice_pump = spawn_auto_answer_pump(alice.clone());
    sleep(Duration::from_millis(300)).await;

    // The agent device itself dials the queue: dispatch must exclude bob
    // and ring alice alone.
    let caller_dialog = bob
        .make_call(QUEUE_NUMBER, Some(sdp.clone()))
        .await
        .unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must be dispatched (the caller bob is excluded from dispatch)",
        |s| matches!(s, AgentStatus::Busy { .. }),
    )
    .await;

    // Bounded negative window: bob never leaves Idle - no ring, no busy,
    // no self-answer (the exclusion held on this dispatch round).
    for _ in 0..10 {
        let bob_now = harness.cc_registry.get_agent("bob").await.unwrap();
        assert!(
            matches!(bob_now.status, AgentStatus::Idle),
            "the caller agent must stay Idle (never self-dispatched), got {}",
            bob_now.status
        );
        sleep(Duration::from_millis(200)).await;
    }

    bob.hangup(&caller_dialog).await.unwrap();
    wait_status(
        &harness.cc_registry,
        "alice",
        Duration::from_secs(8),
        "alice must enter her own Wrapup after the hangup",
        |s| matches!(s, AgentStatus::Wrapup { .. }),
    )
    .await;
    Ok(())
}
