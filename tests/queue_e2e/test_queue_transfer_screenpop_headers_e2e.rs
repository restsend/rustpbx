//! Reproduction (prod 2026-09-30, kefutest cluster): an inbound call routed
//! IVR → `transfer` to `queue:Q?target=skillgroup:SG` (the pre-app-
//! reservation path) delivered the agent-leg INVITE **without** `Call-Info`
//! and **without** `User-to-User`, although the CC enricher ran at queue-app
//! start (`CC enricher: User-to-User header built queue=… uui=q=…;sg=…`).
//! The screen-pop page never opened and the agent card had no queue/skill
//! context.
//!
//! Two scenarios over the same harness (CC enricher attached, desk
//! screen-pop global template configured so the enricher injects BOTH
//! headers):
//! * A — transfer **target override** (`?target=skillgroup:`): the
//!   pre-app-reservation path the production call took.
//! * B — queue **config targets** (`skill-group:` in the queue definition):
//!   the direct entry path (control).
//!
//! Both agent INVITEs MUST carry `User-to-User` (q/sg context) and
//! `Call-Info;purpose=render`.

use anyhow::Result;
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
use rustpbx::addons::cc::{CcAddonState, CcQueueLocationEnricher};
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent, create_test_sdp};

const IVR_NUMBER: &str = "9200";

fn ivr_toml(queue_target: &str) -> String {
    // The greeting file does not exist → tree IVR skips playback and waits
    // for DTMF immediately (no TTS / audio dependency in CI).
    format!(
        r#"
[ivr]
name = "screenpop-headers-ivr"

[ivr.root]
greeting = "sounds/definitely-missing-menu.wav"
timeout_ms = 10000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = {{ type = "transfer", target = "{queue_target}" }}

[[ivr.root.entries]]
key = "9"
action = {{ type = "hangup" }}
"#
    )
}

fn proxy_config(ivr_file: &std::path::Path, queue_name: &str, sg: &str) -> ProxyConfig {
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
        name: Some(queue_name.to_string()),
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Sequential,
            wait_timeout_secs: Some(30),
            targets: vec![RouteQueueTargetConfig {
                uri: format!("skill-group:{sg}"),
                label: None,
            }],
        },
        accept_immediately: false,
        ..Default::default()
    };
    config.queues.insert(queue_name.to_string(), queue_config);

    config.routes = Some(vec![RouteRule {
        name: "route_to_screenpop_ivr".to_string(),
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

/// Harness for one scenario: real server + CC registry/skill group/adapter +
/// CC enricher (desk screen-pop global template, unless `with_template =
/// false`) + IVR/queue routing.
/// `start_busy` flips the agent offline → idle → **busy** (so the call must
/// queue). Returns `(server, harness_registry)` after the server is up.
async fn start_harness(
    scenario: &str,
    queue_name: &str,
    sg: &str,
    transfer_target: &str,
    start_busy: bool,
    with_template: bool,
) -> Result<(E2eTestServer, Arc<CcAgentRegistry>)> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: sg.to_string(),
            display_name: Some("ScreenPop Q".to_string()),
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
    // Presence state machine requires offline → idle before further shifts.
    cc_registry
        .update_status("bob", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();
    if start_busy {
        // Queue-then-assign: the agent must be Busy at entry so the call
        // actually queues and the assignment happens via wait retention.
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
    }

    let adapter = Arc::new(CcAgentRegistryAdapter::new(
        cc_registry.clone(),
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: false,
            ..AcdConfig::default()
        })),
        "localhost",
    ));

    // CC enricher. With the global screen-pop template the enricher injects
    // `Call-Info;purpose=render` (template) AND `User-to-User` (queue/sg
    // context); without ANY configured URL it injects ONLY `User-to-User`
    // (Call-Info 按需).
    let cc_state = CcAddonState::new();
    if with_template {
        cc_state
            .desk_config
            .write()
            .await
            .screen_pop
            .crm_url_template = Some("https://crm/pop?caller=${caller}".into());
    }
    let enricher = CcQueueLocationEnricher::new();
    enricher.attach_state(Arc::new(cc_state));

    let ivr_path = std::env::temp_dir().join(format!(
        "screenpop-headers-ivr-{scenario}-{}.toml",
        portpicker::pick_unused_port().unwrap_or(43000)
    ));
    std::fs::write(&ivr_path, ivr_toml(transfer_target))?;

    let mut proxy_config = proxy_config(&ivr_path, queue_name, sg);
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
            bans: None,
            users,
            agent_registry: Some(adapter),
            queue_enricher: Some(Arc::new(enricher)),
            ..Default::default()
        },
    )
    .await?;

    Ok((server, cc_registry))
}

async fn make_registered_ua(
    server: &E2eTestServer,
    username: &str,
) -> Result<TestUa> {
    let mut ua = TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28000),
        proxy_addr: server.proxy_addr,
    });
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(300)).await;
    Ok(ua)
}

/// Drive one scenario end to end; returns the `(User-to-User, Call-Info)`
/// header values observed on the agent-leg INVITE.
///
/// `queue_then_assign = true` starts the agent **Busy** so the call queues
/// (all_busy) and the agent is assigned later by wait-retention
/// re-resolution (the `dynamic_agents` path) instead of the immediate dial.
async fn run_scenario(
    scenario: &str,
    override_target: bool,
    queue_then_assign: bool,
    with_template: bool,
) -> Result<(Option<String>, Option<String>)> {
    let queue_name = format!("q_{scenario}");
    let sg = format!("sg_{scenario}");
    let transfer_target = if override_target {
        format!("queue:{queue_name}?target=skillgroup:{sg}")
    } else {
        format!("queue:{queue_name}")
    };
    let (server, registry) = start_harness(
        scenario,
        &queue_name,
        &sg,
        &transfer_target,
        queue_then_assign,
        with_template,
    )
    .await?;

    // Agent registered + Idle BEFORE the call: the queue dials immediately.
    let mut bob = make_registered_ua(&server, "bob").await?;

    // Caller → IVR.
    let mut caller = make_registered_ua(&server, "caller").await?;
    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30200),
        false,
    );
    let dialog = caller.make_call(IVR_NUMBER, Some(offer)).await?;
    sleep(Duration::from_millis(600)).await;

    // Press 1 → transfer to the queue.
    caller.send_dtmf_info(&dialog, "1").await?;

    // Queue-then-assign: the agent is Busy → all_busy → wait retention.
    // Flip busy → wrapup → idle so the retention poll assigns him.
    if queue_then_assign {
        sleep(Duration::from_millis(1200)).await;
        registry
            .update_status(
                "bob",
                rustpbx::addons::cc::agent::AgentStatus::Wrapup {
                    call_id: "warmup".to_string(),
                    since: std::time::Instant::now(),
                },
            )
            .await?;
        registry
            .update_status("bob", rustpbx::addons::cc::agent::AgentStatus::Idle)
            .await?;
    }

    // Wait for the agent-leg INVITE and capture its screen-pop headers.
    let mut uui = None;
    let mut call_info = None;
    let mut agent_dialog = None;
    for _ in 0..150 {
        let events = bob.process_dialog_events().await?;
        for ev in events {
            if let TestUaEvent::IncomingCall(id, offer) = ev {
                uui = bob.incoming_invite_header(&id, "User-to-User").await;
                call_info = bob.incoming_invite_header(&id, "Call-Info").await;
                bob.ring_call(&id).await?;
                sleep(Duration::from_millis(300)).await;
                bob.answer_call(
                    &id,
                    offer
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| Some(create_test_sdp("127.0.0.1", portpicker::pick_unused_port().unwrap_or(30201), false))),
                )
                .await?;
                agent_dialog = Some(id);
                break;
            }
        }
        if agent_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let agent_dialog = agent_dialog.expect("queue must dispatch the call to the agent");

    // Let the call connect, then hang up from the agent for clean teardown.
    sleep(Duration::from_millis(800)).await;
    bob.hangup(&agent_dialog).await.ok();
    sleep(Duration::from_millis(300)).await;

    server.stop();
    Ok((uui, call_info))
}

/// Scenario A — the production shape: IVR transfers to
/// `queue:Q?target=skillgroup:SG` (target override → pre-app reservation).
/// The agent INVITE must carry the enricher's screen-pop headers.
#[tokio::test]
async fn queue_transfer_target_override_agent_invite_carries_screenpop_headers() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let (uui, call_info) = run_scenario("override", true, false, true).await?;
    let uui = uui.unwrap_or_else(|| "<missing>".to_string());
    let call_info = call_info.unwrap_or_else(|| "<missing>".to_string());
    assert!(
        uui.contains("q=") && uui.contains("sg="),
        "User-to-User must reach the agent INVITE on the override path: {uui}"
    );
    assert!(
        call_info.contains("https://crm/pop?caller=caller") && call_info.contains("purpose=render"),
        "Call-Info render must reach the agent INVITE on the override path: {call_info}"
    );
    Ok(())
}

/// Scenario B — control: plain queue transfer (queue config targets carry
/// the skill group), agent free → immediate dial. Same headers expected.
#[tokio::test]
async fn queue_direct_entry_agent_invite_carries_screenpop_headers() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let (uui, call_info) = run_scenario("direct", false, false, true).await?;
    let uui = uui.unwrap_or_else(|| "<missing>".to_string());
    let call_info = call_info.unwrap_or_else(|| "<missing>".to_string());
    assert!(
        uui.contains("q=") && uui.contains("sg="),
        "User-to-User must reach the agent INVITE on the direct path: {uui}"
    );
    assert!(
        call_info.contains("https://crm/pop?caller=caller") && call_info.contains("purpose=render"),
        "Call-Info render must reach the agent INVITE on the direct path: {call_info}"
    );
    Ok(())
}

/// Scenario C — **queued then assigned**: agent Busy at entry → all_busy →
/// wait retention re-resolves the skill group and assigns the agent when he
/// goes Idle. The re-resolution builds `dynamic_agents` from bare URIs —
/// the enricher's screen-pop headers must STILL reach the agent INVITE.
#[tokio::test]
async fn queue_wait_retention_assignment_agent_invite_carries_screenpop_headers() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let (uui, call_info) = run_scenario("retention", false, true, true).await?;
    let uui = uui.unwrap_or_else(|| "<missing>".to_string());
    let call_info = call_info.unwrap_or_else(|| "<missing>".to_string());
    assert!(
        uui.contains("q=") && uui.contains("sg="),
        "User-to-User must reach the agent INVITE on the wait-retention path: {uui}"
    );
    assert!(
        call_info.contains("https://crm/pop?caller=caller") && call_info.contains("purpose=render"),
        "Call-Info render must reach the agent INVITE on the wait-retention path: {call_info}"
    );
    Ok(())
}

/// Scenario D — **Call-Info 按需**：no screen-pop URL configured anywhere
/// (no global template / rule / SG metadata / agent extras). The structural
/// guarantee is `User-to-User` ALWAYS present on a dispatched agent leg,
/// while `Call-Info` appears ONLY when a URL is configured — absence of
/// configuration must not produce an empty/broken `Call-Info` header.
#[tokio::test]
async fn queue_dispatch_uui_always_and_call_info_only_when_configured() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let (uui, call_info) = run_scenario("no_template", false, false, false).await?;
    let uui = uui.unwrap_or_else(|| "<missing>".to_string());
    assert!(
        uui.contains("q=") && uui.contains("sg="),
        "User-to-User is unconditional on a dispatched agent leg: {uui}"
    );
    assert!(
        call_info.is_none(),
        "Call-Info must be absent when no screen-pop URL is configured: {call_info:?}"
    );
    Ok(())
}

/// Scenario E — **溢出升级路径**（`dial_agents`，修复点 3）：primary 组坐席
/// 振铃不应答，`max_wait_secs` 到点后 fair widening 把溢出组坐席补进拨打
/// 列表（`dial_agents` 裸 URI originate）。升级腿 INVITE 必须与即时腿
/// 一样携带 enricher 头（修复前该路径 `originate_call` 无头）。
#[tokio::test]
async fn queue_overflow_escalation_agent_invite_carries_screenpop_headers() -> Result<()> {
    use rustpbx::addons::cc::agent::AgentRegistry as InMemoryAgentRegistry;
    use rustpbx::addons::cc::{SkillGroupConfigEntry, SkillGroupTomlCache};
    use std::time::Instant;

    let _ = tracing_subscriber::fmt().try_init();

    // Primary `support` overflows to `support_l2` after 2 queued seconds.
    let mut cache = SkillGroupTomlCache::default();
    cache.groups.insert(
        "support".to_string(),
        SkillGroupConfigEntry {
            skill_group_id: "support".to_string(),
            display_name: None,
            skills_required: vec!["support".to_string()],
            overflow_groups: vec!["support_l2".to_string()],
            sla_target_secs: 30,
            max_wait_secs: 2,
            acd_policy: None,
            overflow_mode: None,
            overflow_after_secs: None,
        },
    );
    cache.groups.insert(
        "support_l2".to_string(),
        SkillGroupConfigEntry {
            skill_group_id: "support_l2".to_string(),
            display_name: None,
            skills_required: vec!["support_l2".to_string()],
            overflow_groups: vec![],
            sla_target_secs: 30,
            max_wait_secs: 90,
            acd_policy: None,
            overflow_mode: None,
            overflow_after_secs: None,
        },
    );

    let cc_registry = Arc::new(InMemoryAgentRegistry::new());
    for (id, skills) in [
        ("agent1", vec!["support"]),
        ("agent2", vec!["support_l2"]),
    ] {
        cc_registry
            .register(id.to_string(), skills.into_iter().map(str::to_string).collect(), 1)
            .await
            .unwrap();
        cc_registry
            .update_status(id, rustpbx::addons::cc::agent::AgentStatus::Idle)
            .await
            .unwrap();
    }

    let adapter = Arc::new(CcAgentRegistryAdapter::new(
        cc_registry,
        Arc::new(AcdEngine::new(AcdConfig::default())),
        "localhost",
    )
    .with_skill_group_cache(Arc::new(tokio::sync::RwLock::new(cache))));

    // CC enricher with the global screen-pop template.
    let cc_state = CcAddonState::new();
    cc_state.desk_config.write().await.screen_pop.crm_url_template =
        Some("https://crm/pop?caller=${caller}".into());
    let enricher = CcQueueLocationEnricher::new();
    enricher.attach_state(Arc::new(cc_state));

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
        name: Some("support".to_string()),
        strategy: RouteQueueStrategyConfig {
            targets: vec![RouteQueueTargetConfig {
                uri: "skill-group:support".to_string(),
                label: None,
            }],
            ..Default::default()
        },
        accept_immediately: false,
        ..Default::default()
    };
    config.queues.insert("support".to_string(), queue_config);
    config.routes = Some(vec![RouteRule {
        name: "route_to_support".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);
    config.ensure_user = Some(false);
    config.enable_latching = false;

    let server = E2eTestServer::start_with_inject(
        config,
        E2eTestServerInject {
            bans: None,
            users: ["caller", "agent1", "agent2"]
                .into_iter()
                .enumerate()
                .map(|(idx, username)| SipUser {
                    id: (idx + 1) as u64,
                    username: username.to_string(),
                    password: Some("password".to_string()),
                    enabled: true,
                    realm: Some("127.0.0.1".to_string()),
                    ..Default::default()
                })
                .collect(),
            agent_registry: Some(adapter),
            queue_enricher: Some(Arc::new(enricher)),
            ..Default::default()
        },
    )
    .await?;
    let proxy_addr = server.proxy_addr;

    let mk_ua = |username: &str, port_hint: u16| {
        TestUa::new(TestUaConfig {
            webrtc: false,
            username: username.to_string(),
            password: "password".to_string(),
            realm: "127.0.0.1".to_string(),
            local_port: portpicker::pick_unused_port().unwrap_or(port_hint),
            proxy_addr,
        })
    };

    // agent1 (primary): receives the INVITE but NEVER answers.
    let mut agent1 = mk_ua("agent1", 26010);
    agent1.start().await?;
    agent1.register().await?;
    // agent2 (overflow): answers once the widened dial reaches it.
    let mut agent2 = mk_ua("agent2", 26011);
    agent2.start().await?;
    agent2.register().await?;
    let mut caller = mk_ua("caller", 26012);
    caller.start().await?;

    let sdp_offer = "v=0\r\n\
        o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio 30001 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
        .to_string();

    let (hangup_tx, hangup_rx) = tokio::sync::oneshot::channel::<()>();
    let call_task = tokio::spawn(async move {
        let dialog_id = caller.make_call("support", Some(sdp_offer)).await?;
        let _ = hangup_rx.await;
        caller.hangup(&dialog_id).await?;
        Ok::<_, anyhow::Error>(())
    });

    // Phase 1: primary agent rings, deliberately unanswered.
    let t0 = Instant::now();
    let mut agent1_dialog = None;
    for _ in 0..80 {
        let events = agent1.process_dialog_events().await?;
        for ev in events {
            if let TestUaEvent::IncomingCall(id, _) = ev {
                agent1.ring_call(&id).await?;
                agent1_dialog = Some(id);
                break;
            }
        }
        if agent1_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(agent1_dialog.is_some(), "primary agent must be dialed first");

    // Phase 2: escalation widening dials agent2 — capture its headers.
    let mut uui = None;
    let mut call_info = None;
    let mut agent2_dialog = None;
    for _ in 0..150 {
        let events = agent2.process_dialog_events().await?;
        for ev in events {
            if let TestUaEvent::IncomingCall(id, offer) = ev {
                uui = agent2.incoming_invite_header(&id, "User-to-User").await;
                call_info = agent2.incoming_invite_header(&id, "Call-Info").await;
                agent2.ring_call(&id).await?;
                sleep(Duration::from_millis(300)).await;
                agent2
                    .answer_call(
                        &id,
                        offer
                            .as_deref()
                            .map(str::to_string)
                            .or_else(|| Some(create_test_sdp("127.0.0.1", portpicker::pick_unused_port().unwrap_or(30202), false))),
                    )
                    .await?;
                agent2_dialog = Some(id);
                break;
            }
        }
        if agent2_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    let agent2_dialog = agent2_dialog.expect("escalation must dial the overflow agent");
    let uui = uui.unwrap_or_else(|| "<missing>".to_string());
    let call_info = call_info.unwrap_or_else(|| "<missing>".to_string());
    assert!(
        uui.contains("q=") && uui.contains("sg="),
        "User-to-User must reach the escalation-dialed agent INVITE: {uui}"
    );
    assert!(
        call_info.contains("https://crm/pop?caller=caller") && call_info.contains("purpose=render"),
        "Call-Info render must reach the escalation-dialed agent INVITE: {call_info}"
    );

    sleep(Duration::from_millis(500)).await;
    hangup_tx.send(()).ok();
    agent2.hangup(&agent2_dialog).await.ok();
    if let Some(id) = agent1_dialog {
        agent1.hangup(&id).await.ok();
    }
    let _ = call_task.await;
    server.stop();
    Ok(())
}
