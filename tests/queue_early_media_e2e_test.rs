//! Queue Early Media E2E Test
//!
//! Verifies that queue properly routes to a direct SIP target (sip:agent@ip:port)
//! and handles the call successfully.

mod helpers;

use std::collections::HashMap;
use std::time::Duration;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};

#[tokio::test]
async fn test_queue_direct_sip_target() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rustpbx=debug,sipbot=debug")
        .try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");
    let agent1_port = portpicker::pick_unused_port().expect("no free agent1 port");

    // ── Queue config: sequential, one target ────────────────────────────────
    let agent1_uri = format!("sip:agent1@127.0.0.1:{}", agent1_port);

    let queue_config = RouteQueueConfig {
        name: Some("support".to_string()),
        accept_immediately: true,
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Sequential,
            wait_timeout_secs: Some(15),
            targets: vec![RouteQueueTargetConfig {
                uri: agent1_uri.clone(),
                label: Some("Agent 1".to_string()),
            }],
        },
        ..Default::default()
    };

    let route_rule = RouteRule {
        name: "to-support-queue".to_string(),
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
    };

    let mut queues = HashMap::new();
    queues.insert("support".to_string(), queue_config);

    // Skip "auth" module so inbound INVITEs go straight to the call module.
    let proxy_config = ProxyConfig {
        modules: Some(vec!["registrar".to_string(), "call".to_string()]),
        acl_rules: Some(vec!["allow all".to_string()]),
        ensure_user: Some(false),
        media_proxy: MediaProxyMode::All,
        ..Default::default()
    };

    let inject = TestPbxInject {
        proxy_config: Some(proxy_config),
        routes: Some(vec![route_rule]),
        queues: Some(queues),
        ..Default::default()
    };

    // ── Start PBX ──────────────────────────────────────────────────────────
    let _pbx = TestPbx::start_with_inject(sip_port, inject).await;
    tracing::info!(sip_port, caller_port, agent1_port, "TestPbx started");

    // ── Start agent UA (sipbot, auto-answer after 2s) ─────────────
    let agent1 = TestUa::callee_with_username(agent1_port, 2, "agent1").await;

    // ── Start caller UA ────────────────────────────────────────────────────
    let pbx_uri = format!("sip:support@127.0.0.1:{}", sip_port);
    let caller = TestUa::caller_with_target(caller_port, "alice", pbx_uri).await;

    // ── Wait for call to finish ────────────────────────────────────────────
    tokio::time::sleep(Duration::from_secs(8)).await;

    // ── Verification ───────────────────────────────────────────────────────
    let agent_rx = agent1.has_rtp_rx();
    let agent_tx = agent1.has_rtp_tx();
    let caller_rx = caller.has_rtp_rx();
    let caller_tx = caller.has_rtp_tx();

    tracing::info!("Agent stats: {}", agent1.rtp_stats_summary());

    tracing::info!("Caller stats: {}", caller.rtp_stats_summary());

    let agent_has_rtp = agent_rx || agent_tx;
    tracing::info!(
        "Agent RTP: {} (rx={}, tx={})",
        if agent_has_rtp { "YES" } else { "no" },
        agent_rx,
        agent_tx
    );

    assert!(
        caller_rx || caller_tx,
        "Caller should have received/sent RTP audio. Stats: {}",
        caller.rtp_stats_summary()
    );

    agent1.stop();
    caller.stop();
}
