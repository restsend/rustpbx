//! Reproduction (prd-mac 2026-11-17, user report): agent A outbound -> echo
//! customer -> single-step blind transfer to the QUEUE feature code. The
//! queue dispatch DIED before the INVITE:
//!
//!   Adding new SIP leg target=sip:1001@...;transport=WSS
//!   WARN Failed to ensure caller leg for app media bridge
//!   WARN Command execution failed: "Queue could not prepare caller media
//!        before dialing"
//!   (no INVITE ever sent — the agent never rang; the 30s no-answer then
//!    pushed the agent client into a break)
//!
//! Root: the dial-time caller-media repair hard-assumes the caller leg
//! exists. An outbound hand-off retires the transferor's CALLER leg and the
//! customer survives as the CALLEE media leg (the queue's waiting music
//! already plays to it) — the repair must not abort the dispatch there.
//!
//! Contract locked here: blind transfer into the queue in the OUTBOUND
//! direction MUST reach the reserved agent (their UA rings).

use anyhow::Result;
use rustpbx::addons::cc::agent::AgentRegistry as CcAgentRegistry;
use rustpbx::addons::cc::agent_registry_adapter::CcAgentRegistryAdapter;
use rustpbx::addons::cc::acd::{AcdConfig, AcdEngine};
use rustpbx::addons::cc::skill_group::CreateSkillGroupRequest;
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

const QUEUE_NUMBER: &str = "8800";
const SG_NAME: &str = "sg_blind_dispatch";

fn proxy_config(queue_name: &str, sg: &str) -> ProxyConfig {
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
    config.ensure_user = Some(false);
    config.enable_latching = false;
    // Anchored media — mirrors the acceptance/deployment topology (the
    // queue app plays waiting music into a per-leg media object).
    config.media_proxy = rustpbx::config::MediaProxyMode::All;

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
        name: "route_queue_direct".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(QUEUE_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some(queue_name.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);
    config
}

async fn start_harness(queue_name: &str, sg: &str) -> Result<E2eTestServer> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: sg.to_string(),
            display_name: Some("Blind Queue Q".to_string()),
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
    // The transferor is a REGISTERED CC agent (the acceptance shape) — the
    // CTI blind transfer's role-aware intercept identifies the agent leg via
    // its CC attribution; an unregistered caller makes it retire the wrong
    // side.
    cc_registry
        .register("alice".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    cc_registry
        .update_status("alice", rustpbx::addons::cc::agent::AgentStatus::Idle)
        .await
        .unwrap();
    cc_registry
        .register("carol".to_string(), vec!["support".to_string()], 1)
        .await
        .unwrap();
    cc_registry
        .update_status("carol", rustpbx::addons::cc::agent::AgentStatus::Idle)
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

    let mut config = proxy_config(queue_name, sg);
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
            username: "carol".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ];
    let _ = &mut config;
    E2eTestServer::start_with_inject(
        config,
        E2eTestServerInject {
            users,
            agent_registry: Some(adapter),
            ..Default::default()
        },
    )
    .await
}

async fn make_registered_ua(server: &E2eTestServer, username: &str) -> Result<TestUa> {
    let mut ua = TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28600),
        proxy_addr: server.proxy_addr,
    });
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(300)).await;
    Ok(ua)
}

async fn answer_next_incoming(
    ua: &TestUa,
    sdp: &str,
    secs: u64,
) -> Result<rsipstack::dialog::DialogId> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        for event in ua.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _) = event {
                ua.answer_call(&id, Some(sdp.to_string())).await?;
                return Ok(id);
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("no incoming call within {secs}s")
}

#[tokio::test]
async fn blind_transfer_into_queue_outbound_reaches_the_reserved_agent() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let queue_name = "q_blind_dispatch";
    let sg = "sg_blind_dispatch";
    let server = start_harness(queue_name, sg).await?;

    let alice = make_registered_ua(&server, "alice").await?; // transferor/agent
    let bob = make_registered_ua(&server, "bob").await?; // customer
    let carol = make_registered_ua(&server, "carol").await?; // reserved queue agent
    let sdp = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30600),
        false,
    );

    // ── agent outbound -> customer answers ───────────────────────────
    let dial = rustpbx::utils::spawn({
        let a = alice.clone();
        let sdp = sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await.unwrap() }
    });
    let bob_dialog = answer_next_incoming(&bob, &sdp, 10).await?;
    let _alice_dialog = dial.await.expect("agent dial must complete");
    sleep(Duration::from_millis(400)).await;
    bob.set_answer_sdp(&bob_dialog, &sdp).await;

    // ── single-step blind transfer into the queue (CTI API — the client's
    //    default 发起即释放 path: the transferor leg is retired at dispatch)
    let handle = server
        .registry
        .get_handle_by_call_id(&_alice_dialog.call_id)
        .ok_or_else(|| anyhow::anyhow!("agent call-id must resolve to its owner session"))?;
    handle
        .send_command(rustpbx::call::domain::CallCommand::BlindTransferCustomer {
            // The CTI entry resolves feature codes BEFORE the command
            // (acceptance trace: target=queue:skill-group:support).
            target: format!("queue:skill-group:{}", SG_NAME),
            headers: Default::default(),
            application_only: false,
            operator_agent: None,
            operator_leg_hint: None,
            completion: None,
        })
        .expect("command dispatch");
    sleep(Duration::from_millis(600)).await;

    // ── the reserved agent (carol) MUST actually ring ────────────────
    // (pre-fix: the dial aborted at the caller-media repair and no INVITE
    // was ever sent — the ring never happened)
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut carol_ringing = false;
    while !carol_ringing && std::time::Instant::now() < deadline {
        for event in carol.process_dialog_events().await? {
            if matches!(event, TestUaEvent::IncomingCall(..)) {
                carol_ringing = true;
            }
        }
        if !carol_ringing {
            sleep(Duration::from_millis(50)).await;
        }
    }
    assert!(
        carol_ringing,
        "the queue dispatch must reach the reserved agent (their UA rings) — \
         the caller-media repair must not abort the dial after an outbound hand-off"
    );

    server.stop();
    Ok(())
}
