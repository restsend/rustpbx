//! Inbound REFER E2E Tests
//!
//! Tests verify that PBX correctly handles incoming REFER requests:
//! - Returns 202 Accepted
//! - Sends NOTIFY 100 Trying and final NOTIFY
//! - Originates new call to transfer target
//! - Bridges original call with transfer target
//! - REFERs to bare numbers routed to a queue/application hand the call to
//!   that flow in-session (with `call_transferred` recording the flow source)

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::TestUaEvent;
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::proxy::routing::{
    MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::webhook_capture::WebhookCapture;

/// Test inbound REFER success flow.
///
/// Scenario:
/// 1. Alice registers and calls Bob via PBX
/// 2. Bob answers
/// 3. Alice sends REFER to PBX, targeting Charlie (sipbot)
/// 4. PBX returns 202 Accepted, then originates to Charlie
/// 5. Charlie answers
/// 6. PBX bridges the calls
#[tokio::test]
async fn test_inbound_refer_success() {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(
        E2eTestServer::start()
            .await
            .expect("E2E server start failed"),
    );

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let bob = server.create_ua("bob").await.expect("create bob failed");

    let alice_sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 12345 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    let bob_sdp = "v=0\r\n\
        o=- 789012 789012 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 54321 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    // Alice calls Bob
    let caller_handle = rustpbx::utils::spawn({
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob receives and answers
    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob
            .process_dialog_events()
            .await
            .expect("process events failed");
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(bob_sdp.clone()))
                    .await
                    .expect("bob answer failed");
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the call");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => panic!("Call failed: {:?}", e),
        _ => panic!("Call timed out"),
    };

    // Wait for active call in registry
    server
        .wait_for_active_call(Duration::from_secs(3))
        .await
        .expect("Call should be in registry");

    // Create Charlie UA (rsipstack-based)
    let charlie = server
        .create_ua("charlie")
        .await
        .expect("create charlie failed");
    let charlie_port = charlie.local_port();
    let charlie_uri = format!("sip:charlie@127.0.0.1:{}", charlie_port);

    let charlie_sdp = "v=0\r\n\
        o=- 111111 111111 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 33333 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    // Spawn a task for Charlie to answer incoming calls
    let charlie_clone = charlie.clone();
    let charlie_answer_handle = rustpbx::utils::spawn(async move {
        let mut charlie_dialog_id = None;
        for _ in 0..50 {
            let events = charlie_clone
                .process_dialog_events()
                .await
                .expect("charlie process events failed");
            for event in events {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    charlie_clone
                        .answer_call(&id, Some(charlie_sdp.clone()))
                        .await
                        .expect("charlie answer failed");
                    charlie_dialog_id = Some(id);
                    break;
                }
            }
            if charlie_dialog_id.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        charlie_dialog_id
    });

    sleep(Duration::from_millis(300)).await;

    // Alice sends REFER to PBX
    let refer_status = alice
        .send_refer(&alice_dialog_id, &charlie_uri)
        .await
        .expect("send_refer failed");

    assert_eq!(refer_status, 202, "REFER should be accepted with 202");

    // Process Alice's dialog events (including NOTIFY from PBX) so the REFER subscription can proceed
    let alice_clone = alice.clone();
    let alice_event_handle = rustpbx::utils::spawn(async move {
        for _ in 0..100 {
            let _ = alice_clone.process_dialog_events().await;
            sleep(Duration::from_millis(50)).await;
        }
    });

    // Wait for Charlie to answer the transfer call
    let charlie_dialog_id = tokio::time::timeout(Duration::from_secs(5), charlie_answer_handle)
        .await
        .expect("Charlie answer timeout")
        .expect("charlie answer task failed");
    assert!(
        charlie_dialog_id.is_some(),
        "Charlie should receive and answer the transfer call"
    );

    alice_event_handle.abort();

    // Wait for PBX to originate to Charlie
    let mut found_transfer_call = false;
    for _ in 0..50 {
        let calls = server.get_active_calls();
        let outbound_calls: Vec<_> = calls
            .iter()
            .filter(|c| {
                c.direction == "outbound"
                    && c.callee
                        .as_ref()
                        .map(|s: &String| s.contains("charlie"))
                        .unwrap_or(false)
            })
            .collect();
        if !outbound_calls.is_empty() {
            found_transfer_call = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found_transfer_call,
        "PBX should originate a call to Charlie after receiving REFER"
    );

    // Cleanup
    alice.hangup(&alice_dialog_id).await.ok();
    bob.hangup(&bob_dialog_id.unwrap()).await.ok();
    if let Some(ref id) = charlie_dialog_id {
        charlie.hangup(id).await.ok();
    }
    server.stop();
}

/// Wait until the webhook capture has seen `event_type`; returns the payload.
async fn wait_webhook_event(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if let Some(ev) = events
                .iter()
                .find(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// A phone-initiated blind transfer (REFER) to a bare number that the route
/// table maps to a queue must hand the call to the queue IN THE ORIGINAL
/// SESSION (gated by `route_originated_calls`) instead of originating a raw
/// call to the number:
///
/// 1. Alice calls Bob; Bob answers.
/// 2. Alice sends REFER to `sip:8000@pbx` where 8000 routes to `refer_queue`.
/// 3. PBX replies 202, dispatches the in-session queue hand-off; the REFER
///    subscription still gets its final NOTIFY.
/// 4. The queue dials its member (agent), agent answers.
/// 5. `call_transferred` lands on the RWI webhook with
///    `transfer_target_type: "queue"` and the original bare number as the
///    target string; `queue_joined` proves the in-session QueueApp started.
/// 6. No outbound leg to the literal number `8000` is ever originated.
#[tokio::test]
async fn test_inbound_refer_to_queue_route() {
    let _ = tracing_subscriber::fmt::try_init();

    const QUEUE_NAME: &str = "refer_queue";
    const REFER_NUMBER: &str = "8000";
    const WEBHOOK_EVENTS: &[&str] = &[
        "call_created",
        "call_ringing",
        "call_answered",
        "call_transferred",
        "queue_joined",
        "queue_agent_offered",
        "queue_agent_connected",
    ];

    let capture = WebhookCapture::start().await;
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(portpicker::pick_unused_port().unwrap_or(15060)),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };
    config.route_originated_calls = true;
    config.queues.insert(
        QUEUE_NAME.to_string(),
        RouteQueueConfig {
            name: Some(QUEUE_NAME.to_string()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "sip:agent@127.0.0.1".to_string(),
                    label: Some("Refer Queue Agent".to_string()),
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "refer_to_queue".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(REFER_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some(QUEUE_NAME.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let server = Arc::new(
        E2eTestServer::start_with_inject(
            config,
            E2eTestServerInject {
                users: vec![
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
                        username: "agent".to_string(),
                        password: Some("password".to_string()),
                        enabled: true,
                        realm: Some("127.0.0.1".to_string()),
                        ..Default::default()
                    },
                ],
                session_hook: None,
                agent_registry: None,
                rwi_gateway: Some(gateway),
            },
        )
        .await
        .expect("E2E server start failed"),
    );

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let bob = server.create_ua("bob").await.expect("create bob failed");
    let agent = server
        .create_ua("agent")
        .await
        .expect("create agent failed");

    let alice_sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 12346 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    let bob_sdp = "v=0\r\n\
        o=- 789013 789013 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 54322 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    // Alice calls Bob
    let caller_handle = rustpbx::utils::spawn({
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob receives and answers
    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob
            .process_dialog_events()
            .await
            .expect("process events failed");
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(bob_sdp.clone()))
                    .await
                    .expect("bob answer failed");
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the call");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => panic!("Call failed: {:?}", e),
        _ => panic!("Call timed out"),
    };

    server
        .wait_for_active_call(Duration::from_secs(3))
        .await
        .expect("Call should be in registry");

    // Agent answers the queue-dispatched call in the background
    let agent_clone = agent.clone();
    let agent_answer_handle = rustpbx::utils::spawn(async move {
        let mut agent_dialog_id = None;
        for _ in 0..100 {
            let events = agent_clone
                .process_dialog_events()
                .await
                .expect("agent process events failed");
            for event in events {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    let sdp_answer = "v=0\r\n\
                        o=agent 3 0 IN IP4 127.0.0.1\r\n\
                        s=agent\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                        m=audio 33334 RTP/AVP 0 101\r\n\
                        a=rtpmap:0 PCMU/8000\r\n\
                        a=rtpmap:101 telephone-event/8000\r\n\
                        a=sendrecv\r\n"
                        .to_string();
                    agent_clone
                        .answer_call(&id, Some(sdp_answer))
                        .await
                        .expect("agent answer failed");
                    agent_dialog_id = Some(id);
                    break;
                }
            }
            if agent_dialog_id.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        agent_dialog_id
    });

    sleep(Duration::from_millis(300)).await;

    // Alice sends REFER targeting the bare queue number
    let refer_target = format!("sip:{}@{}", REFER_NUMBER, server.proxy_addr);
    let refer_status = alice
        .send_refer(&alice_dialog_id, &refer_target)
        .await
        .expect("send_refer failed");
    assert_eq!(refer_status, 202, "REFER should be accepted with 202");

    // Drain Alice's dialog events so the NOTIFY subscription progresses
    let alice_clone = alice.clone();
    let alice_event_handle = rustpbx::utils::spawn(async move {
        for _ in 0..100 {
            let _ = alice_clone.process_dialog_events().await;
            sleep(Duration::from_millis(50)).await;
        }
    });

    // The queue must dial its member
    let agent_dialog_id = tokio::time::timeout(Duration::from_secs(10), agent_answer_handle)
        .await
        .expect("agent answer timeout")
        .expect("agent answer task failed");
    assert!(
        agent_dialog_id.is_some(),
        "Queue member (agent) should receive the transferred call"
    );

    alice_event_handle.abort();

    // The in-session hand-off emits call_transferred annotated with the
    // routed target type, and queue_joined proves the QueueApp started in
    // the original session.
    let transferred = wait_webhook_event(&capture, "call_transferred", Duration::from_secs(5))
        .await
        .expect("webhook must receive call_transferred for the REFER queue hand-off");
    assert_eq!(transferred["transfer_target_type"], "queue");
    assert!(
        transferred["transfer_target"]
            .as_str()
            .is_some_and(|t| t.contains(REFER_NUMBER)),
        "transfer_target must retain the original bare number: {}",
        transferred["transfer_target"]
    );

    let joined = wait_webhook_event(&capture, "queue_joined", Duration::from_secs(5))
        .await
        .expect("webhook must receive queue_joined for the in-session queue start");
    assert_eq!(joined["queue_id"], QUEUE_NAME);

    // No raw originate to the literal number must ever appear.
    sleep(Duration::from_millis(500)).await;
    let dialed_literal = server.get_active_calls().iter().any(|c| {
        c.callee
            .as_deref()
            .is_some_and(|s| s.contains(REFER_NUMBER))
    });
    assert!(
        !dialed_literal,
        "the REFER must not originate a raw call to {REFER_NUMBER}"
    );

    // Cleanup
    alice.hangup(&alice_dialog_id).await.ok();
    bob.hangup(&bob_dialog_id.unwrap()).await.ok();
    if let Some(ref id) = agent_dialog_id {
        agent.hangup(id).await.ok();
    }
    server.stop();
}
