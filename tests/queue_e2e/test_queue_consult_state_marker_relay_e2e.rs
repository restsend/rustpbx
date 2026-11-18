//! Reproduction (prd cluster 2026-11-10, b3w P1b — 3-way over a queue-
//! dispatched consult): the local-merge consult_state marker UPDATE reached
//! the queue session and the relay logged
//! `consult_state UPDATE — relaying marker to opposite side ... side=Caller
//! target_side=Callee`, but the very next line was
//! `Error handling dialog state ... error=No dialog found for leg callee` —
//! the queue/IVR hand-off session never registers a classic **callee** side
//! (the dispatched agent legs live in the leg registry under dynamic UUID
//! ids), so the marker was swallowed and the session auto-replied 501. The
//! consulted agent therefore never saw the merge edge (label stayed on the
//! plain 求助通话 form) although the P2P consult path relays fine.
//!
//! This test pins the queue-dispatched shape: caller → IVR → transfer to a
//! queue → dispatched agent answers → caller sends the marker UPDATE → the
//! agent's UA MUST receive the relayed `consult-state` marker.

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
name = "consult-marker-ivr"

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
        name: "route_to_consult_marker_ivr".to_string(),
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
            display_name: Some("Consult Marker Q".to_string()),
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
        "consult-marker-ivr-{}.toml",
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

#[tokio::test]
async fn test_queue_dispatched_agent_receives_consult_state_marker() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let queue_name = "q_consult_marker";
    let sg = "sg_consult_marker";
    let transfer_target = format!("queue:{queue_name}?target=skillgroup:{sg}");
    let (server, _registry) = start_harness(queue_name, sg, &transfer_target).await?;

    // Agent registered + Idle BEFORE the call: the queue dials immediately.
    let bob = make_registered_ua(&server, "bob").await?;
    let caller = make_registered_ua(&server, "caller").await?;

    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(30300),
        false,
    );
    let dialog = caller.make_call(IVR_NUMBER, Some(offer)).await?;
    sleep(Duration::from_millis(600)).await;
    caller.send_dtmf_info(&dialog, "1").await?;

    // Wait for the dispatched agent-leg INVITE and answer it.
    let mut bob_dialog_id = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while bob_dialog_id.is_none() && std::time::Instant::now() < deadline {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(create_test_sdp("127.0.0.1", 30400, false)))
                    .await?;
                break;
            }
        }
        if bob_dialog_id.is_none() {
            sleep(Duration::from_millis(100)).await;
        }
    }
    let bob_dialog_id = bob_dialog_id.expect("queue must dispatch the agent");
    sleep(Duration::from_millis(500)).await;

    // Caller announces the local 3-way merge edge (consult_state marker).
    caller
        .send_update_with_headers(
            &dialog,
            vec![rsipstack::sip::Header::UserToUser(
                "consult-state=merged".into(),
            )],
        )
        .await?;

    // The dispatched agent MUST receive the relayed marker UPDATE — the
    // queue session has no classic callee side, the relay must fall back to
    // the connected dynamic (dispatched) leg.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = None;
    while std::time::Instant::now() < deadline {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::ConsultStateUpdated(_, phase) = event {
                seen = Some(phase);
            }
        }
        if seen.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        seen.as_deref(),
        Some("merged"),
        "consult_state marker must reach the queue-dispatched agent leg"
    );

    // Exit edge relays as well (attended-transfer / merge-teardown edge).
    caller
        .send_update_with_headers(
            &dialog,
            vec![rsipstack::sip::Header::UserToUser(
                "consult-state=talk".into(),
            )],
        )
        .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut talk_seen = false;
    while std::time::Instant::now() < deadline {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::ConsultStateUpdated(_, phase) = event {
                if phase == "talk" {
                    talk_seen = true;
                }
            }
        }
        if talk_seen {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(
        talk_seen,
        "consult-state=talk must reach the queue-dispatched agent leg"
    );

    caller.hangup(&dialog).await?;
    sleep(Duration::from_millis(300)).await;
    server.stop();
    Ok(())
}
