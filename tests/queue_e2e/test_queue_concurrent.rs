//! Concurrent queue e2e — regression tests for agent double-dial bugs.
//!
//! Reproduces the reported scenario: A+B+C calls enter a queue with 2 agents
//! (bob/alice); after one agent answers, another agent must NOT be dialed
//! into the already-bridged call, and busy agents must not be re-INVITEd by
//! sequential fallback.
//!
//! These tests exercise the production path: `CcAgentRegistryAdapter`
//! (skill-group resolution + atomic agent reservation) wired into a real
//! `SipServer` via `with_agent_registry`, with `TestUa` endpoints
//! registering over real SIP.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::call::VoicePrompts;
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueFallbackConfig,
    RouteQueueStrategyConfig, RouteQueueTargetConfig, RouteRule,
};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{
    TestUa, TestUaConfig, TestUaEvent, create_test_sdp, create_test_sdp_answer,
};

/// Queue extension callers dial to reach the queue.
const QUEUE_NUMBER: &str = "9000";
const SKILL_GROUP: &str = "concurrent_q";
/// Per-agent ring timeout (seconds) — also the queue `wait_timeout_secs`.
const RING_TIMEOUT_SECS: u64 = 2;

// ───────────────────────────────────────────────────────────────────────────
// Harness
// ───────────────────────────────────────────────────────────────────────────

/// Session hook capturing the session-level `resolved_agent_id` at every
/// `on_call_connected` fire, so tests can verify agent attribution.
#[derive(Clone, Default)]
struct AgentCaptureHook {
    captures: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

#[async_trait]
impl CallSessionHook for AgentCaptureHook {
    async fn on_call_connected(&self, ctx: &CallSessionContext) {
        let agent = ctx
            .extensions
            .read()
            .get::<HashMap<String, String>>()
            .and_then(|m| m.get("resolved_agent_id").cloned());
        self.captures
            .lock()
            .await
            .push((ctx.session_id.clone(), agent));
    }
}

/// Live counters for one agent TestUa.
#[derive(Clone, Default)]
struct AgentStats {
    invites: Arc<AtomicUsize>,
    established: Arc<AtomicUsize>,
    terminated: Arc<AtomicUsize>,
    answered: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentStats {
    fn invites(&self) -> usize {
        self.invites.load(Ordering::Relaxed)
    }
}

/// Dial mode + prompt flavor for the queue under test.
#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    /// Sequential, no prompts at all.
    Sequential,
    /// Sequential with a caller-only service prompt after connect (keeps the
    /// queue app alive post-connect — the stale ring-timer window).
    SequentialWithServicePrompt,
    /// Parallel with a caller-only service prompt after connect.
    ParallelWithServicePrompt,
}

impl Flavor {
    fn is_parallel(self) -> bool {
        matches!(self, Flavor::ParallelWithServicePrompt)
    }

    fn has_service_prompt(self) -> bool {
        matches!(
            self,
            Flavor::SequentialWithServicePrompt | Flavor::ParallelWithServicePrompt
        )
    }
}

fn queue_proxy_config(port: u16, flavor: Flavor) -> ProxyConfig {
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

    let voice_prompts = if flavor.has_service_prompt() {
        let none = VoicePrompts {
            transfer_prompt: None,
            busy_prompt: None,
            off_hours_prompt: None,
            no_answer_prompt: None,
            position_prompt: None,
            final_destination_prompt: None,
            comfort_prompts: vec![],
            service_prompt: None,
        };
        Some(VoicePrompts {
            // Resolves to config/sounds/queue-service-zh.wav.
            service_prompt: Some("sounds/queue-service-zh.wav".to_string()),
            ..none
        })
    } else {
        None
    };

    let queue_config = RouteQueueConfig {
        name: Some("concurrent".to_string()),
        strategy: RouteQueueStrategyConfig {
            mode: if flavor.is_parallel() {
                QueueDialMode::Parallel
            } else {
                QueueDialMode::Sequential
            },
            // Maps to the per-agent ring timeout in the queue plan.
            wait_timeout_secs: Some(RING_TIMEOUT_SECS as u16),
            targets: vec![RouteQueueTargetConfig {
                uri: format!("skill-group:{SKILL_GROUP}"),
                label: None,
            }],
        },
        accept_immediately: false,
        voice_prompts,
        fallback: Some(RouteQueueFallbackConfig {
            failure_code: Some(486),
            failure_reason: Some("All agents busy".to_string()),
            redirect: None,
        }),
        ..Default::default()
    };
    config.queues.insert("concurrent".to_string(), queue_config);

    let route = RouteRule {
        name: "route_to_concurrent_queue".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(QUEUE_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("concurrent".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    config.routes = Some(vec![route]);

    config
}

/// Build the CC stack (sqlite memory DB + skill group + 2 idle agents) and
/// return the adapter to inject into the SIP server.
///
/// `bob_idle_first_secs`: when > 0, bob is set Idle first and we wait, then
/// alice — making bob strictly longer-idle so LongestIdle ordering (and the
/// sequential fallback order) is deterministically [bob, alice].
async fn build_cc_adapter(bob_idle_first_secs: u64) -> Arc<CcAgentRegistryAdapter> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: SKILL_GROUP.to_string(),
            display_name: Some("Concurrent Q".to_string()),
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
    for agent_id in ["bob", "alice"] {
        cc_registry
            .register(agent_id.to_string(), vec!["support".to_string()], 1)
            .await
            .unwrap();
        cc_registry
            .update_status(agent_id, rustpbx::addons::cc::agent::AgentStatus::Idle)
            .await
            .unwrap();
        if agent_id == "bob" && bob_idle_first_secs > 0 {
            sleep(Duration::from_secs(bob_idle_first_secs)).await;
        }
    }

    let acd_disabled = Arc::new(AcdEngine::new(AcdConfig {
        enabled: false,
        ..AcdConfig::default()
    }));
    Arc::new(CcAgentRegistryAdapter::new(
        cc_registry,
        acd_disabled,
        "localhost",
    ))
}

struct TestHarness {
    server: E2eTestServer,
    proxy_addr: std::net::SocketAddr,
    captures: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

async fn start_server(port: u16, flavor: Flavor, bob_idle_first_secs: u64) -> Result<TestHarness> {
    let _ = tracing_subscriber::fmt().try_init();

    let adapter = build_cc_adapter(bob_idle_first_secs).await;
    let captures: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::default();
    let hook: Arc<dyn CallSessionHook> = Arc::new(AgentCaptureHook {
        captures: captures.clone(),
    });

    let mut users = Vec::new();
    for (idx, username) in ["bob", "alice", "caller1", "caller2", "caller3"]
        .into_iter()
        .enumerate()
    {
        users.push(SipUser {
            id: (idx + 1) as u64,
            username: username.to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        });
    }

    let server = E2eTestServer::start_with_inject(
        queue_proxy_config(port, flavor),
        E2eTestServerInject {
            users,
            session_hook: Some(hook),
            agent_registry: Some(adapter),
            rwi_gateway: None,
        },
    )
    .await?;

    Ok(TestHarness {
        proxy_addr: server.proxy_addr,
        server,
        captures,
    })
}

fn make_ua(port: u16, proxy_addr: std::net::SocketAddr, username: &str) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        // The fallback port is never used in practice: pick a fresh port so
        // the five scenarios in this file can run CONCURRENTLY under libtest
        // without their UAs fighting over the same hardcoded SIP sockets
        // (a lost bind race makes one test swallow another's INVITEs and
        // corrupts the double-dial counters the assertions rely on).
        local_port: portpicker::pick_unused_port().unwrap_or(port),
        proxy_addr,
    })
}

/// Spawn a pump that drains a registered agent UA's dialog events.
///
/// - Every incoming INVITE increments `stats.invites` (the double-dial
///   detector).
/// - `answer`: answer the FIRST incoming call only (later INVITEs are
///   counted but never answered, exposing the bug).
fn spawn_agent_pump(ua: TestUa, stats: AgentStats, answer: bool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(events) => {
                    for ev in events {
                        match ev {
                            TestUaEvent::IncomingCall(dialog_id, offer) => {
                                stats.invites.fetch_add(1, Ordering::Relaxed);
                                // Answer the FIRST incoming call only; later
                                // INVITEs are counted but never answered, so
                                // a buggy double-dial stays visible.
                                if answer
                                    && !stats
                                        .answered
                                        .swap(true, std::sync::atomic::Ordering::Relaxed)
                                {
                                    let sdp = offer
                                        .as_deref()
                                        .map(|o| create_test_sdp_answer(o, "127.0.0.1", 0))
                                        .unwrap_or_else(|| create_test_sdp("127.0.0.1", 0, false));
                                    if let Err(e) = ua.answer_call(&dialog_id, Some(sdp)).await {
                                        tracing::warn!(error = %e, "agent answer failed");
                                    }
                                }
                            }
                            TestUaEvent::CallEstablished(_) => {
                                stats.established.fetch_add(1, Ordering::Relaxed);
                            }
                            TestUaEvent::CallTerminated(_) => {
                                stats.terminated.fetch_add(1, Ordering::Relaxed);
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

/// Spawn a caller that dials the queue. Resolves with the dialog on 200 OK,
/// or Err when the queue fails the call (486 fallback).
fn spawn_caller(
    ua: TestUa,
    _proxy_addr: std::net::SocketAddr,
    rtp_port: u16,
) -> tokio::task::JoinHandle<Result<rsipstack::dialog::DialogId>> {
    tokio::spawn(async move {
        // Fresh RTP port per call so concurrent scenarios never advertise the
        // same media endpoint (see make_ua for the concurrency rationale).
        let rtp_port = portpicker::pick_unused_port().unwrap_or(rtp_port);
        let offer = create_test_sdp("127.0.0.1", rtp_port, false);
        ua.make_call(QUEUE_NUMBER, Some(offer)).await
    })
}

/// Await a spawned caller, flattening join/timeout/result layers.
/// `make_call` already times out internally (15s).
async fn settle(
    handle: tokio::task::JoinHandle<Result<rsipstack::dialog::DialogId>>,
) -> anyhow::Result<rsipstack::dialog::DialogId> {
    tokio::time::timeout(Duration::from_secs(12), handle)
        .await
        .map_err(|_| anyhow!("caller did not settle within 12s"))?
        .map_err(|e| anyhow!("caller task failed: {e}"))?
}

async fn shutdown(harness: TestHarness, uas: Vec<TestUa>) {
    for ua in uas {
        let _ = ua.stop();
    }
    harness.server.stop();
}

// ───────────────────────────────────────────────────────────────────────────
// Scenario 1: agent answers, stale ring timer must not dial the next agent
// ───────────────────────────────────────────────────────────────────────────

/// A single call enters the queue; bob answers. With a service prompt
/// configured the queue app stays alive past the ring timeout — the exact
/// window where the stale `agent_ring_timeout` used to INVITE the next
/// fallback agent into the bridged call. alice must receive NO INVITE.
#[tokio::test]
async fn test_connected_call_not_dialed_into_after_ring_timeout() -> Result<()> {
    let port = portpicker::pick_unused_port().unwrap_or(16060);
    let harness = start_server(port, Flavor::SequentialWithServicePrompt, 1).await?;

    let mut bob = make_ua(26201, harness.proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let mut alice = make_ua(26202, harness.proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_stats = AgentStats::default();
    let alice_stats = AgentStats::default();
    let bob_pump = spawn_agent_pump(bob.clone(), bob_stats.clone(), true);
    let alice_pump = spawn_agent_pump(alice.clone(), alice_stats.clone(), false);

    let mut caller = make_ua(26211, harness.proxy_addr, "caller1");
    caller.start().await?;
    let call = spawn_caller(caller.clone(), harness.proxy_addr, 30100);

    // Bob answers the first (and only) INVITE.
    let dialog_id = settle(call).await.expect("call should connect via bob");

    // Wait well past the ring timeout (2s) — the stale-timer window.
    sleep(Duration::from_secs(RING_TIMEOUT_SECS + 2)).await;

    assert_eq!(
        bob_stats.invites(),
        1,
        "bob should receive exactly one INVITE"
    );
    assert_eq!(
        alice_stats.invites(),
        0,
        "regression: stale ring timer must not INVITE the next fallback agent after connect"
    );
    assert_eq!(
        bob_stats.established.load(Ordering::Relaxed),
        1,
        "bob's call must still be up"
    );

    let _ = caller.hangup(&dialog_id).await;
    sleep(Duration::from_millis(300)).await;
    bob_pump.abort();
    alice_pump.abort();
    shutdown(harness, vec![caller, bob, alice]).await;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Scenario 2: concurrent calls — fallback must not re-INVITE the busy agent
// ───────────────────────────────────────────────────────────────────────────

/// Two calls enter TRULY concurrently (the reported scenario): both resolves
/// compute the ordered candidate list [bob, alice] before reservations land,
/// so call 1 reserves bob while call 2's fallback list still contains bob.
/// bob answers call 1; alice never answers. When call 2's ring timeout fires,
/// its sequential fallback would re-INVITE bob (already Busy on call 1) — the
/// reported double-dial. Post-fix, call 2 skips busy bob, falls back with
/// 486, and bob receives exactly ONE INVITE.
#[tokio::test]
async fn test_concurrent_fallback_skips_busy_agent() -> Result<()> {
    let port = portpicker::pick_unused_port().unwrap_or(16061);
    let harness = start_server(port, Flavor::Sequential, 1).await?;

    let mut bob = make_ua(26201, harness.proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let mut alice = make_ua(26202, harness.proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_stats = AgentStats::default();
    let alice_stats = AgentStats::default();
    let bob_pump = spawn_agent_pump(bob.clone(), bob_stats.clone(), true);
    let alice_pump = spawn_agent_pump(alice.clone(), alice_stats.clone(), false);

    let mut caller1 = make_ua(26211, harness.proxy_addr, "caller1");
    caller1.start().await?;
    let mut caller2 = make_ua(26212, harness.proxy_addr, "caller2");
    caller2.start().await?;

    // Both calls race in: one reserves bob, the other alice — which caller
    // gets which agent is not deterministic, so assert order-agnostically.
    let call1 = spawn_caller(caller1.clone(), harness.proxy_addr, 30110);
    let call2 = spawn_caller(caller2.clone(), harness.proxy_addr, 30111);

    let r1 = settle(call1).await;
    let r2 = settle(call2).await;
    let (winner_dialog, winner_caller) = match (&r1, &r2) {
        (Ok(d), Err(_)) => (d.clone(), caller1.clone()),
        (Err(_), Ok(d)) => (d.clone(), caller2.clone()),
        _ => panic!(
            "exactly one call should connect via bob, got: {:?} / {:?}",
            r1.is_ok(),
            r2.is_ok()
        ),
    };

    // Let any (buggy) extra INVITE window elapse.
    sleep(Duration::from_secs(RING_TIMEOUT_SECS + 1)).await;

    assert_eq!(
        bob_stats.invites(),
        1,
        "regression: busy bob must not be re-INVITEd by the other call's fallback"
    );
    assert_eq!(
        alice_stats.invites(),
        1,
        "alice is dialed exactly once (the call that reserved her)"
    );
    assert_eq!(
        bob_stats.established.load(Ordering::Relaxed),
        1,
        "bob's connected call must be untouched"
    );
    let _ = r1.is_err(); // silence unused warnings on Result drops
    let _ = r2.is_err();

    let _ = winner_caller.hangup(&winner_dialog).await;
    sleep(Duration::from_millis(300)).await;
    bob_pump.abort();
    alice_pump.abort();
    shutdown(harness, vec![caller1, caller2, bob, alice]).await;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Scenario 3: fallback responder must own the agent attribution
// ───────────────────────────────────────────────────────────────────────────

/// Sequential order is deterministically [bob, alice] (bob set Idle first).
/// bob never answers; the ring timeout falls back to alice, who answers. The
/// session-level `resolved_agent_id` must be corrected to alice (the leg that
/// answered), not stay pinned to bob (the first resolved agent).
#[tokio::test]
async fn test_fallback_answerer_overrides_resolved_agent_id() -> Result<()> {
    let port = portpicker::pick_unused_port().unwrap_or(16062);
    // bob_idle_first_secs=1 → deterministic [bob, alice] ordering.
    let harness = start_server(port, Flavor::Sequential, 1).await?;

    let mut bob = make_ua(26201, harness.proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let mut alice = make_ua(26202, harness.proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_stats = AgentStats::default();
    let alice_stats = AgentStats::default();
    let bob_pump = spawn_agent_pump(bob.clone(), bob_stats.clone(), false);
    // alice answers her first INVITE (the fallback leg).
    let alice_pump = spawn_agent_pump(alice.clone(), alice_stats.clone(), true);

    let mut caller = make_ua(26211, harness.proxy_addr, "caller1");
    caller.start().await?;
    let call = spawn_caller(caller.clone(), harness.proxy_addr, 30120);

    // bob times out (2s) → fallback dials alice → alice answers.
    let dialog = settle(call)
        .await
        .expect("call should connect via alice fallback");

    sleep(Duration::from_millis(300)).await;

    assert_eq!(bob_stats.invites(), 1, "bob dialed first, never answered");
    assert_eq!(alice_stats.invites(), 1, "alice dialed as fallback");

    // Attribution: the LAST on_call_connected capture must name the agent
    // that actually answered (alice), not the first-resolved bob. Each test
    // has exactly one session, so last() is the agent-leg connect.
    let captures_arc = harness.captures.clone();
    let captures = captures_arc.lock().await;
    let agent = captures.last().and_then(|(_, a)| a.clone());
    assert_eq!(
        agent.as_deref(),
        Some("alice"),
        "resolved_agent_id must identify the answering agent (alice), got captures: {captures:?}"
    );

    let _ = caller.hangup(&dialog).await;
    sleep(Duration::from_millis(300)).await;
    bob_pump.abort();
    alice_pump.abort();
    shutdown(harness, vec![caller, bob, alice]).await;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Scenario 4: the reported scenario — A+B+C concurrent, 2 agents
// ───────────────────────────────────────────────────────────────────────────

/// The reported scenario verbatim: A+B+C enter concurrently with 2 agents.
/// A and B race in (one reserves bob, the other alice — bob answers his
/// reserving call, alice never answers); C arrives moments later and finds
/// no available agent. The alice-reserved call's fallback must skip the busy
/// bob instead of bridging a second agent line onto him, and C must fail
/// cleanly.
#[tokio::test]
async fn test_three_calls_two_agents_no_double_dial() -> Result<()> {
    let port = portpicker::pick_unused_port().unwrap_or(16063);
    let harness = start_server(port, Flavor::Sequential, 1).await?;

    let mut bob = make_ua(26201, harness.proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let mut alice = make_ua(26202, harness.proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_stats = AgentStats::default();
    let alice_stats = AgentStats::default();
    let bob_pump = spawn_agent_pump(bob.clone(), bob_stats.clone(), true);
    let alice_pump = spawn_agent_pump(alice.clone(), alice_stats.clone(), false);

    let mut caller_a = make_ua(26211, harness.proxy_addr, "caller1");
    caller_a.start().await?;
    let mut caller_b = make_ua(26212, harness.proxy_addr, "caller2");
    caller_b.start().await?;
    let mut caller_c = make_ua(26213, harness.proxy_addr, "caller3");
    caller_c.start().await?;

    // A and B race in; C arrives after both agents are reserved.
    let call_a = spawn_caller(caller_a.clone(), harness.proxy_addr, 30130);
    let call_b = spawn_caller(caller_b.clone(), harness.proxy_addr, 30131);
    sleep(Duration::from_millis(300)).await;
    let call_c = spawn_caller(caller_c.clone(), harness.proxy_addr, 30132);

    let ra = settle(call_a).await;
    let rb = settle(call_b).await;
    let (winner_dialog, winner_caller) = match (&ra, &rb) {
        (Ok(d), Err(_)) => (d.clone(), caller_a.clone()),
        (Err(_), Ok(d)) => (d.clone(), caller_b.clone()),
        _ => panic!(
            "exactly one of A/B should connect via bob, got: {:?} / {:?}",
            ra.is_ok(),
            rb.is_ok()
        ),
    };

    // C fails: no available agent at resolve time.
    let result_c = settle(call_c).await;
    assert!(result_c.is_err(), "call C must fail: no agent available");
    let _ = ra.is_err();
    let _ = rb.is_err();

    // Let any (buggy) extra INVITE window elapse.
    sleep(Duration::from_secs(RING_TIMEOUT_SECS + 1)).await;

    assert_eq!(
        bob_stats.invites(),
        1,
        "reported scenario: bob answered and must never be re-INVITEd"
    );
    assert_eq!(
        alice_stats.invites(),
        1,
        "alice is dialed exactly once (her reserving call)"
    );
    assert_eq!(
        bob_stats.established.load(Ordering::Relaxed),
        1,
        "the bridged call must remain intact"
    );

    let _ = winner_caller.hangup(&winner_dialog).await;
    sleep(Duration::from_millis(300)).await;
    bob_pump.abort();
    alice_pump.abort();
    shutdown(harness, vec![caller_a, caller_b, caller_c, bob, alice]).await;
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Scenario 5: parallel mode — first answer wins; late timer must not
// abandon the connected call
// ───────────────────────────────────────────────────────────────────────────

/// Parallel dial rings both agents; bob answers, alice's leg is cancelled.
/// With a service prompt the app stays alive past the ring timeout — the
/// stale timer fire used to run the no-answer fallback and tear down the
/// connected call. It must be disarmed on connect.
#[tokio::test]
async fn test_parallel_connected_call_survives_ring_timeout() -> Result<()> {
    let port = portpicker::pick_unused_port().unwrap_or(16064);
    let harness = start_server(port, Flavor::ParallelWithServicePrompt, 0).await?;

    let mut bob = make_ua(26201, harness.proxy_addr, "bob");
    bob.start().await?;
    bob.register().await?;
    let mut alice = make_ua(26202, harness.proxy_addr, "alice");
    alice.start().await?;
    alice.register().await?;
    sleep(Duration::from_millis(300)).await;

    let bob_stats = AgentStats::default();
    let alice_stats = AgentStats::default();
    let bob_pump = spawn_agent_pump(bob.clone(), bob_stats.clone(), true);
    let _alice_pump = spawn_agent_pump(alice.clone(), alice_stats.clone(), false);

    let mut caller = make_ua(26211, harness.proxy_addr, "caller1");
    caller.start().await?;
    let call = spawn_caller(caller.clone(), harness.proxy_addr, 30140);

    let dialog = settle(call)
        .await
        .expect("parallel call should connect via bob");

    // Both agents were INVITEd in parallel mode.
    sleep(Duration::from_millis(200)).await;
    assert_eq!(bob_stats.invites(), 1);
    assert_eq!(alice_stats.invites(), 1, "parallel mode dials both agents");

    // Wait past the ring timeout: the connected call must survive.
    sleep(Duration::from_secs(RING_TIMEOUT_SECS + 2)).await;
    assert_eq!(
        bob_stats.established.load(Ordering::Relaxed),
        1,
        "connected call must still be up after the (disarmed) ring timeout"
    );
    assert_eq!(
        bob_stats.terminated.load(Ordering::Relaxed),
        0,
        "regression: stale parallel ring timer must not abandon the connected call"
    );

    let _ = caller.hangup(&dialog).await;
    sleep(Duration::from_millis(300)).await;
    bob_pump.abort();
    shutdown(harness, vec![caller, bob, alice]).await;
    Ok(())
}
