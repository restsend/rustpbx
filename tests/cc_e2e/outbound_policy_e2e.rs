//! Real-SIP e2e: console-configured 外呼策略 (outbound policy) enforced on
//! the dialout call path. Seeds a CC policy bound to one agent, registers
//! the `CcAgentPolicyRouteInvite` chain node (same wiring as the cc addon's
//! `proxy_server_hook`), and drives real REGISTER + INVITE signaling.
//! Covered: allowed destination forwarded with the client-presented From;
//! destination/caller-id/rule misses → 403; unbound agent passes through.

use anyhow::Result;
use rustpbx::addons::cc::outbound_policy::{self, OutboundPolicy, PolicyLine};
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig};
use sea_orm::{ActiveModelTrait, Database, Set};
use sea_orm_migration::MigratorTrait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};

async fn cc_db_with_policy() -> sea_orm::DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::models::migration::Migrator::up(&db, None).await.unwrap();
    rustpbx::addons::cc::migration::Migrator::up(&db, None).await.unwrap();

    for (id, skills) in [("1001", vec!["support"]), ("1002", vec![])] {
        rustpbx::addons::cc::models::cc_agent::ActiveModel {
            agent_id: Set(id.to_string()),
            display_name: Set(Some(id.to_string())),
            primary_endpoint: Set(Some(id.to_string())),
            skills: Set(serde_json::json!(skills)),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }

    let policy = OutboundPolicy {
        enabled: true,
        lines: vec![
            // Dial "9" + mobile destinations, presenting the agent's own id.
            PolicyLine {
                id: "line-mobile".into(),
                label: "Mobile 外线".into(),
                prefix: "9".into(),
                caller: "self".into(),
                default: true,
                allowed_prefixes: vec!["1".into()],
                ..Default::default()
            },
            // Long-distance line pinned to a specific caller-id.
            PolicyLine {
                id: "line-ld".into(),
                label: "长途".into(),
                prefix: "0".into(),
                caller: "01066660000".into(),
                ..Default::default()
            },
        ],
    };
    outbound_policy::save_policy(&db, "e2e-policy", &policy)
        .await
        .unwrap();
    outbound_policy::apply_policy_targets(&db, "e2e-policy", &[], &["1001".to_string()])
        .await
        .unwrap();
    db
}

async fn start_server() -> Result<(Arc<E2eTestServer>, u16)> {
    let carrier_port = portpicker::pick_unused_port().unwrap_or(26200);

    let mut trunks = HashMap::new();
    trunks.insert(
        "carrier_trunk".to_string(),
        TrunkConfig {
            dest: format!("sip:127.0.0.1:{carrier_port}"),
            ..Default::default()
        },
    );

    // Every fully-dialled number forwards to the trunk, so the policy
    // inspection sees all dialout shapes (allow / deny / no-match).
    let routes = vec![RouteRule {
        name: "route_to_carrier".to_string(),
        priority: 1,
        match_conditions: MatchConditions {
            to_user: Some("^\\d+$".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("forward".to_string()),
            dest: Some(DestConfig::Single("carrier_trunk".to_string())),
            ..Default::default()
        },
        ..Default::default()
    }];

    let config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        trunks,
        routes: Some(routes),
        ..Default::default()
    };

    let inject = E2eTestServerInject {
        users: vec![sip_user("1001", 1), sip_user("1002", 2)],
        cc_policy_db: Some(cc_db_with_policy().await),
        ..Default::default()
    };

    let server = Arc::new(E2eTestServer::start_with_inject(config, inject).await?);
    Ok((server, carrier_port))
}

fn sip_user(name: &str, id: u64) -> rustpbx::call::user::SipUser {
    rustpbx::call::user::SipUser {
        id,
        username: name.to_string(),
        password: Some("password".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        is_support_webrtc: false,
        voicemail_disabled: true,
        ..Default::default()
    }
}

/// Unregistered UA bound to the trunk destination port (the "carrier").
async fn carrier_ua(server: &E2eTestServer, port: u16) -> Result<TestUa> {
    let mut carrier = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "carrier".to_string(),
        password: String::new(),
        realm: "127.0.0.1".to_string(),
        local_port: port,
        proxy_addr: server.proxy_addr,
    });
    carrier.start().await?;
    Ok(carrier)
}

/// Wait for the carrier to receive a forwarded INVITE; returns its dialog id.
async fn wait_incoming(carrier: &TestUa) -> Option<rsipstack::dialog::DialogId> {
    for _ in 0..50 {
        let events = carrier.process_dialog_events().await.ok()?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                return Some(id);
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test]
async fn allowed_destination_forwards_and_establishes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (server, carrier_port) = start_server().await?;
    let carrier = carrier_ua(&server, carrier_port).await?;
    let agent = Arc::new(server.create_ua("1001").await?);
    sleep(Duration::from_millis(200)).await;

    // "self" caller-id → the UA's own From (1001) is allowed; destination
    // 138… starts with the allowed prefix "1".
    let call = {
        let a = agent.clone();
        let sdp = make_sdp(carrier_port);
        tokio::spawn(async move { a.make_call("913800138000", Some(sdp)).await })
    };

    let dialog = wait_incoming(&carrier)
        .await
        .expect("allowed dialout must reach the trunk");
    let from_user = carrier.incoming_from_user(&dialog).await;
    assert_eq!(
        from_user.as_deref(),
        Some("1001"),
        "forwarded INVITE keeps the client-presented From"
    );

    carrier
        .answer_call(&dialog, Some(make_sdp(carrier_port)))
        .await?;
    let call_id = tokio::time::timeout(Duration::from_secs(8), call).await;
    assert!(
        matches!(&call_id, Ok(Ok(Ok(_)))),
        "agent call must establish: {call_id:?}"
    );

    agent.hangup(&call_id.unwrap().unwrap().unwrap()).await.ok();
    server.stop();
    Ok(())
}

#[tokio::test]
async fn disallowed_destination_is_forbidden() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (server, carrier_port) = start_server().await?;
    let carrier = carrier_ua(&server, carrier_port).await?;
    let agent = Arc::new(server.create_ua("1001").await?);
    sleep(Duration::from_millis(200)).await;

    // 200… does not start with the rule's allowed prefix "1" → 403.
    let result = agent
        .make_call("92000123456", Some(make_sdp(carrier_port)))
        .await;
    let err = result.err().expect("policy must deny the call");
    assert!(err.to_string().contains("403"), "want 403, got: {err}");
    assert!(
        wait_incoming(&carrier).await.is_none(),
        "denied call must never reach the trunk"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn mismatched_caller_id_is_forbidden() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (server, carrier_port) = start_server().await?;
    let carrier = carrier_ua(&server, carrier_port).await?;
    let agent = Arc::new(server.create_ua("1001").await?);
    sleep(Duration::from_millis(200)).await;

    // The long-distance line pins caller-id 01066660000; the agent presents
    // its own id (1001) → denied by the caller-id check.
    let result = agent
        .make_call("02155551234", Some(make_sdp(carrier_port)))
        .await;
    let err = result.err().expect("caller-id mismatch must deny the call");
    assert!(err.to_string().contains("403"), "want 403, got: {err}");
    assert!(wait_incoming(&carrier).await.is_none());

    server.stop();
    Ok(())
}

#[tokio::test]
async fn number_matching_no_rule_is_forbidden() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (server, carrier_port) = start_server().await?;
    let carrier = carrier_ua(&server, carrier_port).await?;
    let agent = Arc::new(server.create_ua("1001").await?);
    sleep(Duration::from_millis(200)).await;

    // The route table forwards it, but no policy line matches "555…" → 403.
    let result = agent
        .make_call("5551234567", Some(make_sdp(carrier_port)))
        .await;
    let err = result.err().expect("unmatched dial must deny the call");
    assert!(err.to_string().contains("403"), "want 403, got: {err}");
    assert!(wait_incoming(&carrier).await.is_none());

    server.stop();
    Ok(())
}

#[tokio::test]
async fn unbound_agent_passes_through() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (server, carrier_port) = start_server().await?;
    let carrier = carrier_ua(&server, carrier_port).await?;
    let agent = Arc::new(server.create_ua("1002").await?);
    sleep(Duration::from_millis(200)).await;

    // No policy bound to 1002 → plain passthrough, no caller-id constraint.
    let call = {
        let a = agent.clone();
        let sdp = make_sdp(carrier_port);
        tokio::spawn(async move { a.make_call("913800138000", Some(sdp)).await })
    };

    let dialog = wait_incoming(&carrier)
        .await
        .expect("unbound agent dialout must pass through");
    let from_user = carrier.incoming_from_user(&dialog).await;
    assert_eq!(from_user.as_deref(), Some("1002"));

    carrier
        .answer_call(&dialog, Some(make_sdp(carrier_port)))
        .await?;
    let call_id = tokio::time::timeout(Duration::from_secs(8), call).await;
    assert!(matches!(&call_id, Ok(Ok(Ok(_)))), "call must establish: {call_id:?}");

    agent.hangup(&call_id.unwrap().unwrap().unwrap()).await.ok();
    server.stop();
    Ok(())
}
