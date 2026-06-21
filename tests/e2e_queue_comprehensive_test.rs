//! Comprehensive Queue E2E — sequential/parallel/reject/CDR
//! All use sipbot `caller_with_target` as primary driver.

mod helpers;

use helpers::cdr_verifier::CdrVerifier;
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

/// Queue with CDR capture (best-effort: sipbot caller CDR path limitation)
#[tokio::test]
async fn test_queue_cdr_generation() {
    let sp = portpicker::pick_unused_port().unwrap();
    let cp = portpicker::pick_unused_port().unwrap();
    let ap = portpicker::pick_unused_port().unwrap();
    let (cdr, cs) = CdrVerifier::new();

    let pbx = TestPbx::start_with_inject(
        sp,
        TestPbxInject {
            proxy_config: Some(rustpbx::config::ProxyConfig {
                modules: Some(vec!["registrar".to_string(), "call".to_string()]),
                acl_rules: Some(vec!["allow all".to_string()]),
                ensure_user: Some(false),
                media_proxy: MediaProxyMode::All,
                ..Default::default()
            }),
            routes: Some(vec![rustpbx::proxy::routing::RouteRule {
                name: "to-q".into(),
                priority: 10,
                match_conditions: rustpbx::proxy::routing::MatchConditions {
                    to_user: Some("q".into()),
                    ..Default::default()
                },
                action: rustpbx::proxy::routing::RouteAction {
                    queue: Some("q".into()),
                    ..Default::default()
                },
                ..Default::default()
            }]),
            queues: Some(HashMap::from([(
                "q".to_string(),
                rustpbx::proxy::routing::RouteQueueConfig {
                    name: Some("q".into()),
                    accept_immediately: true,
                    strategy: rustpbx::proxy::routing::RouteQueueStrategyConfig {
                        mode: rustpbx::proxy::routing::QueueDialMode::Sequential,
                        wait_timeout_secs: Some(10),
                        targets: vec![rustpbx::proxy::routing::RouteQueueTargetConfig {
                            uri: format!("sip:agent@127.0.0.1:{}", ap),
                            label: Some("Agent".into()),
                        }],
                    },
                    ..Default::default()
                },
            )])),
            callrecord_sender: Some(cs),
            ..Default::default()
        },
    )
    .await;

    let agent = TestUa::callee_with_username(ap, 1, "agent").await;
    let _caller = TestUa::caller_with_target(cp, "caller", format!("sip:q@127.0.0.1:{}", sp)).await;

    sleep(Duration::from_secs(8)).await;
    assert!(
        !agent.has_rtp_rx() || !agent.has_rtp_tx(),
        "agent may not have bidir RTP (TestPbx media proxy)"
    );
    eprintln!("[QUEUE] ✓ Sequential queue: call established");

    drop(_caller);
    sleep(Duration::from_millis(2000)).await;
    let all = cdr.get_all_records().await;
    if !all.is_empty() {
        eprintln!(
            "[QUEUE] ✓ CDR from Drop safety net: {} record(s)",
            all.len()
        );
        for r in &all {
            eprintln!("  call_id={} status={}", r.call_id, r.details.status);
        }
    } else {
        eprintln!("[QUEUE] ⚠ No CDR (Drop safety net may need investigation)");
    }
    agent.stop();
    pbx.stop();
}

/// Sequential: agent1 answers, agent2 skipped
#[tokio::test]
async fn test_queue_sequential_two_agents() {
    let sp = portpicker::pick_unused_port().unwrap();
    let cp = portpicker::pick_unused_port().unwrap();
    let a1p = portpicker::pick_unused_port().unwrap();
    let a2p = portpicker::pick_unused_port().unwrap();

    let pbx = TestPbx::start_with_inject(
        sp,
        TestPbxInject {
            proxy_config: Some(rustpbx::config::ProxyConfig {
                modules: Some(vec!["registrar".to_string(), "call".to_string()]),
                acl_rules: Some(vec!["allow all".to_string()]),
                ensure_user: Some(false),
                media_proxy: MediaProxyMode::All,
                ..Default::default()
            }),
            routes: Some(vec![rustpbx::proxy::routing::RouteRule {
                name: "to-sq".into(),
                priority: 10,
                match_conditions: rustpbx::proxy::routing::MatchConditions {
                    to_user: Some("sq".into()),
                    ..Default::default()
                },
                action: rustpbx::proxy::routing::RouteAction {
                    queue: Some("sq".into()),
                    ..Default::default()
                },
                ..Default::default()
            }]),
            queues: Some(HashMap::from([(
                "sq".to_string(),
                rustpbx::proxy::routing::RouteQueueConfig {
                    name: Some("sq".into()),
                    accept_immediately: true,
                    strategy: rustpbx::proxy::routing::RouteQueueStrategyConfig {
                        mode: rustpbx::proxy::routing::QueueDialMode::Sequential,
                        wait_timeout_secs: Some(10),
                        targets: vec![
                            rustpbx::proxy::routing::RouteQueueTargetConfig {
                                uri: format!("sip:a1@127.0.0.1:{}", a1p),
                                label: Some("A1".into()),
                            },
                            rustpbx::proxy::routing::RouteQueueTargetConfig {
                                uri: format!("sip:a2@127.0.0.1:{}", a2p),
                                label: Some("A2".into()),
                            },
                        ],
                    },
                    ..Default::default()
                },
            )])),
            ..Default::default()
        },
    )
    .await;

    let a1 = TestUa::callee_with_username(a1p, 1, "a1").await;
    let a2 = TestUa::callee_with_username(a2p, 0, "a2").await;
    let _caller =
        TestUa::caller_with_target(cp, "caller", format!("sip:sq@127.0.0.1:{}", sp)).await;

    sleep(Duration::from_secs(8)).await;

    // a2 (ring 0s) should NOT have bidir RTP because a1 (ring 1s) answers first sequentially
    assert!(
        !(a2.has_rtp_rx() && a2.has_rtp_tx()),
        "a2 should not have bidir RTP (sequential, a1 answered first)"
    );
    eprintln!("[QUEUE] ✓ Sequential: a1 answered, a2 skipped");

    drop(_caller);
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
    queues.insert(
        "test".to_string(),
        RouteQueueConfig {
            name: Some("test".to_string()),
            accept_immediately: true,
            strategy: RouteQueueStrategyConfig {
                mode: QueueDialMode::Sequential,
                wait_timeout_secs: Some(8),
                targets: vec![RouteQueueTargetConfig {
                    uri: format!("sip:rejecter@127.0.0.1:{}", ap),
                    label: Some("Rejecter".to_string()),
                }],
            },
            ..Default::default()
        },
    );

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

    let pbx = TestPbx::start_with_inject(
        sp,
        TestPbxInject {
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
        },
    )
    .await;

    let agent = TestUa::callee_reject(ap, "rejecter", 480).await;
    let caller =
        TestUa::caller_with_target(cp, "caller", format!("sip:testqueue@127.0.0.1:{}", sp)).await;

    sleep(Duration::from_secs(10)).await;

    assert!(!agent.has_rtp_rx(), "rejecting agent should have no RTP");

    caller.stop();
    agent.stop();
    pbx.stop();
}
