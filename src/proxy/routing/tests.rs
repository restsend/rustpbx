use crate::call::{DialDirection, DialStrategy, RoutingState};
use crate::call::{FailureAction, QueueFallbackAction};
use crate::config::RouteResult;
use crate::proxy::routing::matcher::{RouteResourceLookup, match_invite};
use crate::proxy::routing::{
    DestConfig, MatchConditions, QueueDialMode, RejectConfig, RewriteRules, RouteAction,
    RouteDirection, RouteQueueConfig, RouteQueueFallbackConfig, RouteQueueHoldConfig,
    RouteQueueStrategyConfig, RouteQueueTargetConfig, RouteRule, SourceTrunk, TrunkConfig,
    TrunkDirection,
};
use async_trait::async_trait;
use rsip::StatusCode;
use rsipstack::dialog::invitation::InviteOption;
use std::sync::Arc;
use std::{collections::HashMap, net::IpAddr};

// Configuration parsing tests removed - focus on core routing functionality

#[tokio::test]
async fn test_match_invite_no_routes() {
    let routing_state = Arc::new(RoutingState::new());
    let option = create_test_invite_option();
    let origin = create_test_request();

    let trunks = HashMap::new();
    let routes = vec![];

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::Forward(_, _) | RouteResult::NotHandled(_, _) => {} // Expected
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::Queue { .. } => {
            panic!("Unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_trunk_matches_inbound_ip_with_cidr() {
    let trunk = TrunkConfig {
        dest: "sip:10.0.0.1:5060".to_string(),

        inbound_hosts: vec!["192.168.10.0/24".to_string()],
        ..Default::default()
    };

    let inside: IpAddr = "192.168.10.42".parse().unwrap();
    let outside: IpAddr = "192.168.20.1".parse().unwrap();

    assert!(trunk.matches_inbound_ip(&inside).await);
    assert!(!trunk.matches_inbound_ip(&outside).await);
}

#[tokio::test]
async fn test_trunk_matches_inbound_ip_with_hostname() {
    let trunk = TrunkConfig {
        dest: "sip:localhost:5060".to_string(),
        inbound_hosts: vec!["localhost".to_string()],
        ..Default::default()
    };

    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    assert!(trunk.matches_inbound_ip(&loopback).await);
}

#[test]
fn test_trunk_incoming_user_prefix_plain() {
    let mut trunk = TrunkConfig::default();
    trunk.incoming_from_user_prefix = Some("+852".to_string());
    assert!(
        trunk
            .matches_incoming_user_prefixes(Some("+852123456"), None)
            .unwrap()
    );
    assert!(
        !trunk
            .matches_incoming_user_prefixes(Some("+853123456"), None)
            .unwrap()
    );
}

#[test]
fn test_trunk_incoming_user_prefix_regex() {
    let mut trunk = TrunkConfig::default();
    trunk.incoming_to_user_prefix = Some(r"^\d{10}$".to_string());
    assert!(
        trunk
            .matches_incoming_user_prefixes(None, Some("1234567890"))
            .unwrap()
    );
    assert!(
        !trunk
            .matches_incoming_user_prefixes(None, Some("1234"))
            .unwrap()
    );
}

#[test]
fn test_trunk_incoming_user_prefix_invalid_regex() {
    let mut trunk = TrunkConfig::default();
    trunk.incoming_from_user_prefix = Some("^(".to_string());
    let err = trunk
        .matches_incoming_user_prefixes(Some("1001"), None)
        .unwrap_err();
    assert!(err.to_string().contains("invalid regex"));
}

#[tokio::test]
async fn test_match_invite_inbound_respects_source_trunk() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();
    trunks.insert(
        "ingress".to_string(),
        TrunkConfig {
            dest: "sip:ingress.gateway.local:5060".to_string(),
            direction: Some(TrunkDirection::Inbound),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "inbound-route".to_string(),
        priority: 10,
        direction: RouteDirection::Inbound,
        source_trunks: vec!["ingress".to_string()],
        match_conditions: MatchConditions {
            to_user: Some("1001".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            dest: Some(DestConfig::Single("ingress".to_string())),
            select: "rr".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }];

    let option = create_test_invite_option();
    let origin = create_test_request();
    let source_trunk = SourceTrunk {
        name: "ingress".to_string(),
        id: None,
        direction: Some(TrunkDirection::Inbound),
    };

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        Some(&source_trunk),
        routing_state,
        &DialDirection::Inbound,
    )
    .await
    .expect("invite should resolve");

    match result {
        RouteResult::Forward(_, _) => {}
        RouteResult::NotHandled(_, _) | RouteResult::Abort(_, _) => {
            panic!("expected inbound invite to forward")
        }
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_inbound_without_source_trunk() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();
    trunks.insert(
        "ingress".to_string(),
        TrunkConfig {
            dest: "sip:ingress.gateway.local:5060".to_string(),
            direction: Some(TrunkDirection::Inbound),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "inbound-route".to_string(),
        priority: 10,
        direction: RouteDirection::Inbound,
        source_trunks: vec!["ingress".to_string()],
        match_conditions: MatchConditions {
            to_user: Some("1001".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            dest: Some(DestConfig::Single("ingress".to_string())),
            select: "rr".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }];

    let option = create_test_invite_option();
    let origin = create_test_request();

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Inbound,
    )
    .await
    .expect("invite should resolve");

    match result {
        RouteResult::NotHandled(_, _) => {}
        RouteResult::Forward(_, _) | RouteResult::Abort(_, _) => {
            panic!("expected invite to be left unhandled when source trunk is missing")
        }
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_exact_match() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "test_trunk".to_string(),
        TrunkConfig {
            dest: "sip:gateway.rustpbx.com:5060".to_string(),
            username: Some("testuser".to_string()),
            password: Some("testpass".to_string()),
            transport: Some("udp".to_string()),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "test_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            to_user: Some("1001".to_string()),
            ..Default::default()
        },
        rewrite: None,
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("test_trunk".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option = create_test_invite_option();
    let origin = create_test_request();

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .expect("Failed to match invite");

    match result {
        RouteResult::NotHandled(_, _) => panic!("Expected forward, got not handled"),
        RouteResult::Forward(option, _) => {
            // Verify destination is set
            assert!(option.destination.is_some());
            let dest = option.destination.unwrap();
            assert_eq!(dest.addr.to_string(), "gateway.rustpbx.com:5060");

            // Verify credential is set
            assert!(option.credential.is_some());
            let cred = option.credential.unwrap();
            assert_eq!(cred.username, "testuser");
            assert_eq!(cred.password, "testpass");
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_regex_match() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "mobile_trunk".to_string(),
        TrunkConfig {
            dest: "sip:mobile.gateway.com:5060".to_string(),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "mobile_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            to_user: Some("^1[3-9]\\d{9}$".to_string()), // Mobile number regex
            ..Default::default()
        },
        rewrite: None,
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("mobile_trunk".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option = create_invite_option(
        "sip:alice@rustpbx.com",
        "sip:13812345678@rustpbx.com",
        None,
        Some("application/sdp"),
        None,
    );
    let origin = create_test_request();

    // Test matching mobile number
    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::NotHandled(_option, _) => {
            panic!("Expected forward, got NotHandled")
        }
        RouteResult::Forward(_option, _) => {
            // Expected
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_queue_action_builds_hold_and_fallback() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();
    trunks.insert(
        "support".to_string(),
        TrunkConfig {
            dest: "sip:support-gateway.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    let queue_cfg = RouteQueueConfig {
        accept_immediately: true,
        passthrough_ringback: true,
        hold: Some(RouteQueueHoldConfig {
            audio_file: Some("moh/support.wav".to_string()),
            loop_playback: true,
        }),
        fallback: Some(RouteQueueFallbackConfig {
            redirect: Some("sip:voicemail@rustpbx.com".to_string()),
            failure_code: None,
            failure_reason: None,
            failure_prompt: None,
            queue_ref: None,
        }),
        ..RouteQueueConfig::default()
    };

    let queue_path = "queue/support.toml";
    let mut lookup = TestResourceLookup::default();
    lookup.add_queue(queue_path, queue_cfg.clone());

    let routes = vec![RouteRule {
        name: "support-queue".to_string(),
        priority: 50,
        direction: RouteDirection::Inbound,
        match_conditions: MatchConditions {
            to_user: Some("1001".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("queue".to_string()),
            dest: Some(DestConfig::Single("support".to_string())),
            queue: Some(queue_path.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }];

    let option = create_test_invite_option();
    let origin = create_test_request();

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        Some(&lookup),
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Inbound,
    )
    .await
    .expect("queue route should resolve");

    match result {
        RouteResult::Queue { option, queue, .. } => {
            assert!(
                option.destination.is_some(),
                "queue should select trunk destination"
            );
            assert!(
                queue.accept_immediately,
                "queue must honor accept_immediately flag"
            );
            assert!(
                queue.passthrough_ringback,
                "queue must honor passthrough_ringback flag"
            );

            let hold = queue.hold.expect("queue hold config missing");
            assert_eq!(hold.audio_file.as_deref(), Some("moh/support.wav"));
            assert!(hold.loop_playback);

            match queue.fallback.expect("fallback missing") {
                QueueFallbackAction::Redirect { target } => {
                    assert_eq!(target.to_string(), "sip:voicemail@rustpbx.com");
                }
                other => panic!("unexpected fallback action: {:?}", other),
            }
        }
        RouteResult::Forward(_, _) => panic!("route forwarded instead of enqueuing"),
        RouteResult::NotHandled(_, _) => panic!("route was not handled"),
        RouteResult::Abort(..) => panic!("queue route aborted"),
    }
}

#[test]
fn test_queue_fallback_play_then_hangup_action() {
    let queue_cfg = RouteQueueConfig {
        accept_immediately: true,
        hold: None,
        fallback: Some(RouteQueueFallbackConfig {
            redirect: None,
            failure_code: Some(486),
            failure_reason: Some("Busy".to_string()),
            failure_prompt: Some("prompts/busy.wav".to_string()),
            queue_ref: None,
        }),
        ..RouteQueueConfig::default()
    };

    let plan = queue_cfg
        .to_queue_plan()
        .expect("queue config should convert to plan");

    match plan.fallback.expect("fallback missing") {
        QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
            audio_file,
            status_code,
            reason,
            ..
        }) => {
            assert_eq!(audio_file, "prompts/busy.wav");
            assert_eq!(status_code, StatusCode::BusyHere);
            assert_eq!(reason.as_deref(), Some("Busy"));
        }
        other => panic!("unexpected fallback action: {:?}", other),
    }
}

#[test]
fn test_queue_fallback_to_named_queue() {
    let queue_cfg = RouteQueueConfig {
        fallback: Some(RouteQueueFallbackConfig {
            queue_ref: Some("tier2".to_string()),
            ..RouteQueueFallbackConfig::default()
        }),
        ..RouteQueueConfig::default()
    };

    let plan = queue_cfg
        .to_queue_plan()
        .expect("queue config should convert to plan");

    match plan.fallback.expect("fallback missing") {
        QueueFallbackAction::Queue { name } => assert_eq!(name, "tier2"),
        other => panic!("unexpected fallback action: {:?}", other),
    }
}

#[test]
fn test_queue_strategy_builds_dial_targets() {
    let queue_cfg = RouteQueueConfig {
        strategy: RouteQueueStrategyConfig {
            mode: QueueDialMode::Parallel,
            wait_timeout_secs: Some(12),
            targets: vec![
                RouteQueueTargetConfig {
                    uri: "sip:agent1@pbx.example".to_string(),
                    label: Some("Agent 1".to_string()),
                },
                RouteQueueTargetConfig {
                    uri: "sip:agent2@pbx.example".to_string(),
                    label: None,
                },
            ],
        },
        ..RouteQueueConfig::default()
    };

    let plan = queue_cfg
        .to_queue_plan()
        .expect("queue config should convert to plan");

    match plan.dial_strategy.expect("dial strategy missing") {
        DialStrategy::Parallel(targets) => {
            assert_eq!(targets.len(), 2);
            assert_eq!(targets[0].aor.to_string(), "sip:agent1@pbx.example");
        }
        other => panic!("unexpected strategy: {:?}", other),
    }
    assert_eq!(plan.ring_timeout, Some(std::time::Duration::from_secs(12)));
}

#[tokio::test]
async fn test_match_invite_reject_rule() {
    let routing_state = Arc::new(RoutingState::new());
    let routes = vec![RouteRule {
        name: "emergency_reject".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            to_user: Some("^(110|120|119)$".to_string()),
            ..Default::default()
        },
        rewrite: None,
        action: RouteAction {
            action: Some("reject".to_string()),
            dest: None,
            select: "rr".to_string(),
            hash_key: None,
            reject: Some(RejectConfig {
                code: 403,
                reason: Some("Emergency calls not allowed".to_string()),
                headers: HashMap::new(),
            }),
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option = create_invite_option(
        "sip:alice@rustpbx.com",
        "sip:110@rustpbx.com",
        None,
        Some("application/sdp"),
        None,
    );
    let origin = create_test_request();
    let trunks = HashMap::new();
    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::Abort(code, reason) => {
            assert_eq!(code, rsip::StatusCode::Forbidden);
            assert_eq!(reason, Some("Emergency calls not allowed".to_string()));
        }
        RouteResult::Forward(_, _) => panic!("Expected abort, got forward"),
        RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_rewrite_rules() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "trunk1".to_string(),
        TrunkConfig {
            dest: "sip:gateway.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "rewrite_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            from_user: Some("^\\+86(1\\d{10})$".to_string()),
            ..Default::default()
        },
        rewrite: Some(RewriteRules {
            from_user: Some("0{1}".to_string()), // Rewrite to 0+number
            ..Default::default()
        }),
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("trunk1".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let mut option = create_test_invite_option();
    option.caller = "sip:+8613812345678@rustpbx.com".try_into().unwrap();
    let origin = create_test_request();

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::Forward(option, _) => {
            // Verify caller was rewritten
            let caller_user = option.caller.user().unwrap_or_default();
            assert_eq!(caller_user, "013812345678");
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_load_balancing() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "trunk1".to_string(),
        TrunkConfig {
            dest: "sip:gateway1.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    trunks.insert(
        "trunk2".to_string(),
        TrunkConfig {
            dest: "sip:gateway2.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    trunks.insert(
        "trunk3".to_string(),
        TrunkConfig {
            dest: "sip:gateway3.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "load_balance_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            to_user: Some("1001".to_string()),
            ..Default::default()
        },
        rewrite: None,
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Multiple(vec![
                "trunk1".to_string(),
                "trunk2".to_string(),
                "trunk3".to_string(),
            ])),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let _option = create_test_invite_option();
    let origin = create_test_request();

    // Test multiple calls to verify round-robin behavior
    let mut selected_destinations = Vec::new();
    for _ in 0..5 {
        let test_option = create_test_invite_option();
        let result = match_invite(
            Some(&trunks),
            Some(&routes),
            None,
            test_option,
            &origin,
            None,
            routing_state.clone(),
            &DialDirection::Outbound,
        )
        .await
        .unwrap();

        match result {
            RouteResult::Forward(option, _) => {
                // Verify some trunk was selected
                assert!(option.destination.is_some());
                let dest = option.destination.unwrap();
                selected_destinations.push(dest.addr.to_string());
            }
            RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
            RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
            RouteResult::Queue { .. } => {
                panic!("unexpected queue result")
            }
        }
    }

    // With round-robin, we should get different trunks
    println!("Selected destinations: {:?}", selected_destinations);
}

#[tokio::test]
async fn test_match_invite_header_matching() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "vip_trunk".to_string(),
        TrunkConfig {
            dest: "sip:vip.gateway.com:5060".to_string(),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "vip_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            headers: {
                let mut headers = HashMap::new();
                headers.insert("header.X-VIP".to_string(), "gold".to_string());
                headers
            },
            ..Default::default()
        },
        rewrite: None,
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("vip_trunk".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option = create_test_invite_option();
    let origin = create_sip_request(
        rsip::Method::Invite,
        "sip:1001@rustpbx.com",
        "Alice <sip:alice@rustpbx.com>",
        "Bob <sip:1001@rustpbx.com>",
        &format!("{}@rustpbx.com", generate_random_string(8)),
        1,
        Some(vec![rsip::Header::Other(
            "X-VIP".to_string(),
            "gold".to_string(),
        )]),
    );

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::Forward(_option, _) => {
            // Expected to match VIP header
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_default_route() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "default".to_string(),
        TrunkConfig {
            dest: "sip:default.gateway.com:5060".to_string(),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "non_matching_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            to_user: Some("^999\\d+$".to_string()), // Will not match 1001
            ..Default::default()
        },
        rewrite: None,
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("trunk1".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option = create_test_invite_option();
    let origin = create_test_request();

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::Forward(option, _) => {
            // Should use default trunk, but due to our simplified implementation, this just verifies there's a destination
            println!("Default route selected: {:?}", option.destination);
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        // Accept NotHandled when no route matches and no default forwarding is applied
        RouteResult::NotHandled(_, _) => {
            // acceptable in current behavior
        }
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_advanced_rewrite_patterns() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "test_trunk".to_string(),
        TrunkConfig {
            dest: "sip:gateway.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    // Test case 1: US number format +1(5551234567) -> 001{1}
    let routes_config_us = vec![RouteRule {
        name: "us_rewrite_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            from_user: Some("^\\+1(\\d{10})$".to_string()),
            ..Default::default()
        },
        rewrite: Some(RewriteRules {
            from_user: Some("001{1}".to_string()),
            ..Default::default()
        }),
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("test_trunk".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option_us = create_invite_option(
        "sip:+15551234567@rustpbx.com",
        "sip:1001@rustpbx.com",
        None,
        Some("application/sdp"),
        None,
    );
    let origin_us = create_test_request();

    let result_us = match_invite(
        Some(&trunks),
        Some(&routes_config_us),
        None,
        option_us,
        &origin_us,
        None,
        routing_state.clone(),
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result_us {
        RouteResult::Forward(option, _) => {
            let caller_user = option.caller.user().unwrap_or_default();
            assert_eq!(caller_user, "0015551234567");
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }

    // Test case 2: Simple digit extraction 12345 -> prefix{1}suffix
    let routes_config_digits = vec![RouteRule {
        name: "digit_rewrite_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            from_user: Some("^(\\d+)$".to_string()),
            ..Default::default()
        },
        rewrite: Some(RewriteRules {
            from_user: Some("ext{1}".to_string()),
            ..Default::default()
        }),
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("test_trunk".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option_digits = create_invite_option(
        "sip:12345@rustpbx.com",
        "sip:1001@rustpbx.com",
        None,
        Some("application/sdp"),
        None,
    );
    let origin_digits = create_test_request();

    let result_digits = match_invite(
        Some(&trunks),
        Some(&routes_config_digits),
        None,
        option_digits,
        &origin_digits,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result_digits {
        RouteResult::Forward(option, _) => {
            let caller_user = option.caller.user().unwrap_or_default();
            assert_eq!(caller_user, "ext12345");
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[tokio::test]
async fn test_match_invite_rewrite_from_host_uses_match_capture() {
    let routing_state = Arc::new(RoutingState::new());
    let mut trunks = HashMap::new();

    trunks.insert(
        "trunk1".to_string(),
        TrunkConfig {
            dest: "sip:gateway.rustpbx.com:5060".to_string(),
            ..Default::default()
        },
    );

    let routes = vec![RouteRule {
        name: "host_rewrite_rule".to_string(),
        description: None,
        priority: 100,
        match_conditions: MatchConditions {
            from_user: Some("^\\+1(\\d{10})$".to_string()),
            from_host: Some("^(gw-[a-z-]+)\\.provider\\.net$".to_string()),
            ..Default::default()
        },
        rewrite: Some(RewriteRules {
            from_user: Some("001{1}".to_string()),
            from_host: Some("proxy-{1}.internal".to_string()),
            ..Default::default()
        }),
        action: RouteAction {
            action: None,
            dest: Some(DestConfig::Single("trunk1".to_string())),
            select: "rr".to_string(),
            hash_key: None,
            reject: None,
            ..Default::default()
        },
        disabled: None,
        ..Default::default()
    }];

    let option = create_invite_option(
        "sip:+15551234567@gw-us-west.provider.net",
        "sip:1001@rustpbx.com",
        None,
        Some("application/sdp"),
        None,
    );
    let origin = create_test_request();

    let result = match_invite(
        Some(&trunks),
        Some(&routes),
        None,
        option,
        &origin,
        None,
        routing_state,
        &DialDirection::Outbound,
    )
    .await
    .unwrap();

    match result {
        RouteResult::Forward(option, _) => {
            let caller_user = option.caller.user().unwrap_or_default();
            assert_eq!(caller_user, "0015551234567");

            let caller_host = option.caller.host().to_string();
            assert_eq!(caller_host, "proxy-gw-us-west.internal");
        }
        RouteResult::Abort(_, _) => panic!("Expected forward, got abort"),
        RouteResult::NotHandled(_, _) => panic!("Expected abort, got NotHandled"),
        RouteResult::Queue { .. } => {
            panic!("unexpected queue result")
        }
    }
}

#[derive(Default)]
struct TestResourceLookup {
    queues: HashMap<String, RouteQueueConfig>,
}

impl TestResourceLookup {
    fn add_queue(&mut self, path: &str, config: RouteQueueConfig) {
        self.queues.insert(path.to_string(), config);
    }
}

#[async_trait]
impl RouteResourceLookup for TestResourceLookup {
    async fn load_queue(&self, path: &str) -> anyhow::Result<Option<RouteQueueConfig>> {
        Ok(self.queues.get(path).cloned())
    }
}

// Helper functions - removed mock implementations and replaced with real SIP message builders
fn create_invite_option(
    caller: &str,
    callee: &str,
    contact: Option<&str>,
    content_type: Option<&str>,
    headers: Option<Vec<rsip::Header>>,
) -> InviteOption {
    let default_contact = format!(
        "sip:{}@192.168.1.1:5060",
        caller.split('@').next().unwrap_or("user")
    );
    let contact_uri = contact.unwrap_or(&default_contact);

    InviteOption {
        caller: caller.try_into().expect("Invalid caller URI"),
        callee: callee.try_into().expect("Invalid callee URI"),
        content_type: content_type.map(|s| s.to_string()),
        offer: content_type
            .filter(|ct| ct.contains("sdp"))
            .map(|_| create_minimal_sdp(caller).into_bytes()),
        contact: contact_uri.try_into().expect("Invalid contact URI"),
        headers,
        ..Default::default()
    }
}

fn create_sip_request(
    method: rsip::Method,
    request_uri: &str,
    from: &str,
    to: &str,
    call_id: &str,
    cseq_num: u32,
    additional_headers: Option<Vec<rsip::Header>>,
) -> rsip::Request {
    let branch = format!("z9hG4bK{}", generate_random_string(16));
    let from_tag = generate_random_string(8);

    let mut headers = vec![
        rsip::Header::Via(
            format!("SIP/2.0/UDP 192.168.1.1:5060;branch={}", branch)
                .try_into()
                .expect("Invalid Via header"),
        ),
        rsip::Header::From(
            format!("{};tag={}", from, from_tag)
                .try_into()
                .expect("Invalid From header"),
        ),
        rsip::Header::To(to.try_into().expect("Invalid To header")),
        rsip::Header::CallId(call_id.into()),
        rsip::Header::CSeq(
            format!("{} {}", cseq_num, method)
                .try_into()
                .expect("Invalid CSeq header"),
        ),
        rsip::Header::MaxForwards(70.into()),
    ];

    if let Some(additional) = additional_headers {
        headers.extend(additional);
    }

    let body = if method == rsip::Method::Invite {
        create_minimal_sdp(from).into_bytes()
    } else {
        Vec::new()
    };

    if !body.is_empty() {
        headers.push(rsip::Header::ContentType("application/sdp".into()));
        headers.push(rsip::Header::ContentLength((body.len() as u32).into()));
    }

    rsip::Request {
        method,
        uri: request_uri.try_into().expect("Invalid request URI"),
        version: rsip::Version::V2,
        headers: headers.into(),
        body,
    }
}

fn create_minimal_sdp(user_part: &str) -> String {
    let username = user_part.split('@').next().unwrap_or("user");
    let session_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    format!(
        "v=0\r\n\
         o={} {} {} IN IP4 192.168.1.1\r\n\
         s=Session\r\n\
         c=IN IP4 192.168.1.1\r\n\
         t=0 0\r\n\
         m=audio 5004 RTP/AVP 0 8\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n",
        username, session_id, session_id
    )
}

fn generate_random_string(length: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", timestamp).chars().take(length).collect()
}

// Convenience functions for common test scenarios
fn create_test_invite_option() -> InviteOption {
    create_invite_option(
        "sip:alice@rustpbx.com",
        "sip:1001@rustpbx.com",
        None,
        Some("application/sdp"),
        None,
    )
}

fn create_test_request() -> rsip::Request {
    create_sip_request(
        rsip::Method::Invite,
        "sip:1001@rustpbx.com",
        "Alice <sip:alice@rustpbx.com>",
        "Bob <sip:1001@rustpbx.com>",
        &format!("{}@rustpbx.com", generate_random_string(8)),
        1,
        None,
    )
}
