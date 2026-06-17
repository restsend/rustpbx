//! Parallel Queue E2E Test
//!
//! Verifies that when a queue is configured with `QueueDialMode::Parallel`,
//! the PBX forks INVITEs to all targets concurrently and bridges the first
//! agent that answers, cancelling the remaining forks.
//!
//! Flow:
//! 1. Caller calls `sip:sales@pbx` (matches route → queue "sales")
//! 2. PBX answers caller immediately (accept_immediately = true)
//! 3. PBX forks INVITE to agent1 AND agent2 in parallel
//! 4. Agent2 answers first → PBX bridges caller ↔ agent2
//! 5. Agent1 receives CANCEL for its pending INVITE
//! 6. RTP audio flows between caller and the connected agent

mod helpers;

use std::collections::HashMap;
use std::time::Duration;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};

#[tokio::test]
async fn test_parallel_queue_fork_first_answer_wins() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rustpbx=debug,sipbot=debug")
        .try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");
    let agent1_port = portpicker::pick_unused_port().expect("no free agent1 port");
    let agent2_port = portpicker::pick_unused_port().expect("no free agent2 port");

    // ── Queue config: parallel, two targets ────────────────────────────────
    let agent1_uri = format!("sip:agent1@127.0.0.1:{}", agent1_port);
    let agent2_uri = format!("sip:agent2@127.0.0.1:{}", agent2_port);

    let queue_config = RouteQueueConfig {
        name: Some("sales".to_string()),
        accept_immediately: true,
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Parallel,
            wait_timeout_secs: Some(15),
            targets: vec![
                RouteQueueTargetConfig {
                    uri: agent1_uri,
                    label: Some("Agent 1".to_string()),
                },
                RouteQueueTargetConfig {
                    uri: agent2_uri,
                    label: Some("Agent 2".to_string()),
                },
            ],
        },
        ..Default::default()
    };

    let route_rule = RouteRule {
        name: "to-sales-queue".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("sales".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("sales".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };

    let mut queues = HashMap::new();
    queues.insert("sales".to_string(), queue_config);

    // Skip "auth" module so inbound INVITEs go straight to the call module.
    let proxy_config = ProxyConfig {
        modules: Some(vec!["registrar".to_string(), "call".to_string()]),
        acl_rules: Some(vec!["allow all".to_string()]),
        ensure_user: Some(false),
        ..Default::default()
    };

    let inject = TestPbxInject {
        proxy_config: Some(proxy_config),
        routes: Some(vec![route_rule]),
        queues: Some(queues),
        ..Default::default()
    };

    // ── Start PBX ──────────────────────────────────────────────────────────
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;
    tracing::info!(
        sip_port,
        caller_port,
        agent1_port,
        agent2_port,
        "TestPbx started"
    );

    // ── Start agent UAs (sipbot, auto-answer after short ring) ─────────────
    // Agent1 rings 2 s before answering — its fork should be cancelled.
    let agent1 = TestUa::callee_with_username(agent1_port, 2, "agent1").await;
    // Agent2 rings 0 s — answers immediately (first to answer wins).
    let agent2 = TestUa::callee_with_username(agent2_port, 0, "agent2").await;

    // ── Caller dials the queue ─────────────────────────────────────────────
    let caller = TestUa::caller_with_target(
        caller_port,
        "caller",
        format!("sip:sales@127.0.0.1:{}", sip_port),
    )
    .await;

    tracing::info!("Caller placed call, waiting for queue to fork and agent to answer…");

    // Give the system time to:
    //  1. route the INVITE to the queue
    //  2. fork to both agents
    //  3. agent2 answers immediately (ring_secs=0)
    //  4. PBX bridges caller ↔ agent2, cancels agent1
    //  5. RTP flows
    tokio::time::sleep(Duration::from_secs(12)).await;

    // ── Assertions ─────────────────────────────────────────────────────────

    // Agent2 (ring_secs=0) should have answered and have RTP activity.
    let agent2_rx = agent2.has_rtp_rx();
    let agent2_tx = agent2.has_rtp_tx();
    let agent2_quality = agent2.audio_quality_summary();
    tracing::info!(
        agent2_rx,
        agent2_tx,
        "Agent2 RTP stats: {}, quality: total={} silence={}",
        agent2.rtp_stats_summary(),
        agent2_quality.total_frames,
        agent2_quality.silence_frames
    );
    assert!(
        agent2_rx || agent2_tx,
        "Agent2 should have RTP activity (answered the parallel fork)"
    );

    // Agent1 (ring_secs=2) should NOT have answered — its fork was cancelled.
    let agent1_rx = agent1.has_rtp_rx();
    let agent1_tx = agent1.has_rtp_tx();
    tracing::info!(
        agent1_rx,
        agent1_tx,
        "Agent1 RTP stats: {}",
        agent1.rtp_stats_summary()
    );
    assert!(
        !(agent1_rx && agent1_tx),
        "Agent1 should NOT have bidirectional RTP (its fork should have been cancelled)"
    );

    // Caller should also have RTP activity (bridged with agent2).
    let caller_rx = caller.has_rtp_rx();
    let caller_tx = caller.has_rtp_tx();
    tracing::info!(
        caller_rx,
        caller_tx,
        "Caller RTP stats: {}",
        caller.rtp_stats_summary()
    );
    assert!(
        caller_rx || caller_tx,
        "Caller should have RTP activity (bridged with agent)"
    );

    // ── Cleanup ────────────────────────────────────────────────────────────
    agent1.stop();
    agent2.stop();
    caller.stop();
    pbx.stop();

    tracing::info!("Parallel queue E2E test completed!");
}
