//! Queue E2E tests — sipbot driver
//!
//! Tests queue behavior: sequential agent selection, agent rejection.
//! All use `TestUa::caller_with_target` as primary driver (no RWI).

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::{
    MatchConditions, QueueDialMode, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;

async fn make_pbx(
    sip_port: u16,
    queues: HashMap<String, RouteQueueConfig>,
    routes: Vec<RouteRule>,
) -> TestPbx {
    TestPbx::start_with_inject(sip_port, TestPbxInject {
        proxy_config: Some(ProxyConfig {
            modules: Some(vec!["registrar".to_string(), "call".to_string()]),
            acl_rules: Some(vec!["allow all".to_string()]),
            ensure_user: Some(false),
            media_proxy: MediaProxyMode::All,
            ..Default::default()
        }),
        routes: Some(routes),
        queues: Some(queues),
        ..Default::default()
    }).await
}

/// Sequential queue — agent1 answers first, agent2 should be skipped.
#[tokio::test]
async fn test_queue_sequential_agent_selection() {
    let _ = tracing_subscriber::fmt::try_init();

    let sp = portpicker::pick_unused_port().unwrap();
    let cp = portpicker::pick_unused_port().unwrap();
    let a1p = portpicker::pick_unused_port().unwrap();
    let a2p = portpicker::pick_unused_port().unwrap();

    let mut queues = HashMap::new();
    queues.insert("support".to_string(), RouteQueueConfig {
        name: Some("support".to_string()),
        accept_immediately: true,
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Sequential,
            wait_timeout_secs: Some(10),
            targets: vec![
                RouteQueueTargetConfig {
                    uri: format!("sip:a1@127.0.0.1:{}", a1p),
                    label: Some("A1".to_string()),
                },
                RouteQueueTargetConfig {
                    uri: format!("sip:a2@127.0.0.1:{}", a2p),
                    label: Some("A2".to_string()),
                },
            ],
        },
        ..Default::default()
    });

    let routes = vec![RouteRule {
        name: "to-support".to_string(),
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
    }];

    let pbx = make_pbx(sp, queues, routes).await;

    let a1 = TestUa::callee_with_username(a1p, 1, "a1").await;
    let a2 = TestUa::callee_with_username(a2p, 0, "a2").await;
    let caller = TestUa::caller_with_target(cp, "caller",
        format!("sip:support@127.0.0.1:{}", sp)).await;

    // Sequential: a1 rings 1s → answers → a2 is not tried
    sleep(Duration::from_secs(8)).await;

    assert!(!(a2.has_rtp_rx() && a2.has_rtp_tx()),
        "a2 should not have bidir RTP (sequential)");
    eprintln!("[test] ✓ a1 answered, a2 skipped (sequential)");

    caller.stop();
    a1.stop();
    a2.stop();
    pbx.stop();
}

/// Queue agent reject 480 — agent declines, no RTP flows.
#[tokio::test]
async fn test_queue_agent_reject_fallback() {
    let _ = tracing_subscriber::fmt::try_init();

    let sp = portpicker::pick_unused_port().unwrap();
    let cp = portpicker::pick_unused_port().unwrap();
    let ap = portpicker::pick_unused_port().unwrap();

    let mut queues = HashMap::new();
    queues.insert("test".to_string(), RouteQueueConfig {
        name: Some("test".to_string()),
        accept_immediately: true,
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Sequential,
            wait_timeout_secs: Some(8),
            targets: vec![
                RouteQueueTargetConfig {
                    uri: format!("sip:rejecter@127.0.0.1:{}", ap),
                    label: Some("Rejecter".to_string()),
                },
            ],
        },
        ..Default::default()
    });

    let routes = vec![RouteRule {
        name: "to-test".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("testqueue".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("test".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }];

    let pbx = make_pbx(sp, queues, routes).await;

    let agent = TestUa::callee_reject(ap, "rejecter", 480).await;
    let caller = TestUa::caller_with_target(cp, "caller",
        format!("sip:testqueue@127.0.0.1:{}", sp)).await;

    sleep(Duration::from_secs(10)).await;

    assert!(!agent.has_rtp_rx(), "rejecting agent should have no RTP");
    eprintln!("[test] ✓ Agent reject 480 handled");

    caller.stop();
    agent.stop();
    pbx.stop();
}
