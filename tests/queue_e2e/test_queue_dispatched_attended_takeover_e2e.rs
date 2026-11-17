//! Reproduction (acceptance 2026-11-16, cases 4b91d5c450b2a767 /
//! 86292e54cda4274a — complete-transfer to a queue-dispatched consult target):
//! the transferor consults the QUEUE/IVR feature code, the queue dispatches
//! agent B who ANSWERS, the transferor completes the transfer with
//! `Refer-To: <feature-code>?Replaces=<consult dialog>` (the client's
//! 289a439 Refer-To fallback for an internal-leg consult dialog).
//!
//! Defect (pre-fix): the R1 stage resolved the feature code to
//! `queue:skill-group:<sg>` and the in-session hand-off PREEMPTED the
//! Replaces takeover — the answered consult was torn (B hung), the
//! transferor exited, and the customer was re-queued on hold music forever.
//! Log fingerprint: `REFER hand-off via quick-route feature code` +
//! `attended=true app_target="queue:skill-group:..."`.
//!
//! Contract (post-fix): a consult that has already been answered by a
//! REGISTERED AGENT must complete via the RFC 3891 takeover chain (here:
//! the transfer-room fallback — a plain TestUa customer carries no
//! restsend UA, so the relay is not available). The consult target stays
//! bridged to the customer; NO hand-off happens. The un-dispatched shape
//! (consult still ringing inside the IVR) keeps the R1 hand-off semantics
//! — locked by `test_attended_refer_to_ivr_moves_customer_into_app`.

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

const IVR_NUMBER: &str = "9300";

fn ivr_toml(queue_target: &str) -> String {
    // The greeting file does not exist → tree IVR skips playback and waits
    // for DTMF immediately (no TTS / audio dependency in CI).
    format!(
        r#"
[ivr]
name = "queue-complete-ivr"

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
        name: "route_to_queue_complete_ivr".to_string(),
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

async fn start_harness(
    queue_name: &str,
    sg: &str,
    transfer_target: &str,
) -> Result<(E2eTestServer, Arc<CcAgentRegistry>)> {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None)
        .await
        .unwrap();

    rustpbx::addons::cc::skill_group::create_skill_group(
        &db,
        CreateSkillGroupRequest {
            skill_group_id: sg.to_string(),
            display_name: Some("Queue Complete Q".to_string()),
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

    let adapter = Arc::new(CcAgentRegistryAdapter::new(
        cc_registry.clone(),
        Arc::new(AcdEngine::new(AcdConfig {
            enabled: false,
            ..AcdConfig::default()
        })),
        "localhost",
    ));

    let ivr_path = std::env::temp_dir().join(format!(
        "queue-complete-ivr-{}.toml",
        portpicker::pick_unused_port().unwrap_or(43100)
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
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 3,
            username: "cust".to_string(),
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
            agent_registry: Some(adapter),
            ..Default::default()
        },
    )
    .await?;

    Ok((server, cc_registry))
}

async fn make_registered_ua(server: &E2eTestServer, username: &str) -> Result<TestUa> {
    let mut ua = TestUa::new(TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(28100),
        proxy_addr: server.proxy_addr,
    });
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(300)).await;
    Ok(ua)
}

/// Answer the next incoming call on `ua`, returning its dialog id.
async fn answer_next_incoming(ua: &TestUa, sdp: &str, secs: u64) -> Result<rsipstack::dialog::DialogId> {
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
async fn attended_refer_to_answered_queue_dispatch_takes_over_instead_of_handoff() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let queue_name = "q_complete_takeover";
    let sg = "sg_complete_takeover";
    let transfer_target = format!("queue:{queue_name}?target=skillgroup:{sg}");
    let (server, _registry) = start_harness(queue_name, sg, &transfer_target).await?;

    let bob = make_registered_ua(&server, "bob").await?;
    let alice = make_registered_ua(&server, "alice").await?;
    let cust = make_registered_ua(&server, "cust").await?;
    let sdp = create_test_sdp("127.0.0.1", portpicker::pick_unused_port().unwrap_or(30300), false);

    // ── customer ↔ transferor (direct call, both answer) ─────────────
    // make_call blocks until the 200 OK — answer the callee concurrently.
    let dial = rustpbx::utils::spawn({
        let c = cust.clone();
        let sdp = sdp.clone();
        async move { c.make_call("alice", Some(sdp)).await.unwrap() }
    });
    let alice_main = answer_next_incoming(&alice, &sdp, 10).await?;
    let cust_dialog = dial.await.expect("customer dial must complete");
    sleep(Duration::from_millis(500)).await;
    // TestUa answers server-initiated re-INVITEs (the transfer-room join)
    // with the per-dialog saved SDP; without it the 200 OK carries no body
    // and the room join fails on SDP parse.
    cust.set_answer_sdp(&cust_dialog, &sdp).await;
    alice.set_answer_sdp(&alice_main, &sdp).await;
    assert_eq!(
        server.server_ref.active_call_registry.count(),
        1,
        "customer-transferor session must be up"
    );

    // ── transferor consults the QUEUE feature code → dispatch bob ────
    let consult = alice.make_call(IVR_NUMBER, Some(sdp.clone())).await?;
    sleep(Duration::from_millis(600)).await;
    alice.send_dtmf_info(&consult, "1").await?;
    let bob_dialog = answer_next_incoming(&bob, &sdp, 15).await?;
    sleep(Duration::from_millis(700)).await;
    bob.set_answer_sdp(&bob_dialog, &sdp).await;
    alice.set_answer_sdp(&consult, &sdp).await;
    assert_eq!(
        server.server_ref.active_call_registry.count(),
        2,
        "consult session must be up with the dispatched agent"
    );

    // ── attended REFER with the FEATURE-CODE Refer-To (bug shape) ────
    let replaces = format!(
        "{};to-tag={};from-tag={}",
        consult.call_id, consult.remote_tag, consult.local_tag
    );
    let target = format!(
        "sip:{IVR_NUMBER}@{}?Replaces={}",
        server.proxy_addr,
        urlencoding::encode(&replaces)
    );
    assert_eq!(alice.send_refer(&alice_main, &target).await?, 202);

    // GREEN: the takeover chain (room fallback — plain TestUa has no
    // restsend UA) bridges the customer to the dispatched agent. The
    // transfer room must exist and gather both parties.
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    let mut room = None;
    while std::time::Instant::now() < deadline {
        room = server
            .server_ref
            .conference_server
            .list_conferences_detail()
            .await
            .into_iter()
            .find(|r| r.id.0.starts_with("transfer-"));
        if room.as_ref().is_some_and(|r| r.participant_count() >= 2) {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let room = room.expect("takeover must create a transfer room bridging customer and target");
    assert!(
        room.participant_count() >= 2,
        "customer and dispatched agent must both be in the transfer room, got {}",
        room.participant_count()
    );

    // The dispatched agent's dialog must survive (the pre-fix hand-off tore
    // it: bob saw CallTerminated while the customer re-queued on MOH).
    let mut bob_torn = false;
    for _ in 0..20 {
        for event in bob.process_dialog_events().await? {
            if matches!(event, TestUaEvent::CallTerminated(_)) {
                bob_torn = true;
            }
        }
        if bob_torn {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !bob_torn,
        "the answered consult target must stay bridged — the hand-off must not tear it"
    );

    server.stop();
    Ok(())
}
