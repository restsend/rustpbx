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
use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode, ProxyConfig};
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
/// 3. Bob sends REFER on the outbound callee dialog, targeting Charlie
/// 4. PBX returns 202 Accepted, then originates to Charlie
/// 5. Charlie answers
/// 6. PBX bridges the calls
#[tokio::test]
async fn test_inbound_refer_success() {
    let _ = tracing_subscriber::fmt::try_init();

    for (mode, transferor_bye) in [(MediaProxyMode::All, true), (MediaProxyMode::Auto, true), (MediaProxyMode::All, false)] {
    let mut config = crate::common::test_helpers::test_proxy_config(portpicker::pick_unused_port().unwrap());
    config.media_proxy = mode;
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject {
                queue_enricher: None,
        users, ..Default::default()
    }).await.expect("E2E server start failed"));

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

    // The caller must answer the re-INVITE when a bypass call is anchored
    // at the PBX for the new dynamic target.
    alice.set_answer_sdp(&alice_dialog_id, &alice_sdp).await;

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
    let charlie_reinvite_sdp = charlie_sdp.clone();
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

    let bob_dialog = bob_dialog_id.as_ref().unwrap();
    let registry = &server.server_ref.active_call_registry;
    let owner = registry.get_handle_by_dialog(&bob_dialog.call_id)
        .expect("B-leg bare Call-ID must resolve for REFER");
    assert_ne!(owner.session_id(), bob_dialog.call_id);
    assert_eq!(registry.get_handle_by_call_id(&bob_dialog.call_id).unwrap().session_id(), owner.session_id(),
        "CTI and REFER must use the same registry key");

    // REFER arrives on B's dialog, whose Call-ID differs from the session ID.
    let refer_status = bob
        .send_refer(bob_dialog_id.as_ref().unwrap(), &charlie_uri)
        .await
        .expect("send_refer failed");

    assert_eq!(refer_status, 202, "REFER should be accepted with 202");

    // Process the transferor's NOTIFY requests so the subscription can proceed.
    let bob_clone = bob.clone();
    let bob_event_handle = rustpbx::utils::spawn(async move {
        for _ in 0..100 {
            for event in bob_clone.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(_, body, state) if state.starts_with("terminated") => return body,
                    TestUaEvent::CallTerminated(_) => panic!("PBX must not hang up the transferor"),
                    _ => {}
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("successful transfer must send a final NOTIFY");
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

    let final_notify = tokio::time::timeout(Duration::from_secs(5), bob_event_handle)
        .await.unwrap().unwrap();
    assert!(final_notify.starts_with("SIP/2.0 200"), "{final_notify}");

    let c = charlie_dialog_id.as_ref().unwrap();
    let c_at_pbx = rsipstack::dialog::DialogId {
        call_id: c.call_id.clone(), local_tag: c.remote_tag.clone(), remote_tag: c.local_tag.clone(),
    };
    let (reply, result) = tokio::sync::oneshot::channel();
    owner.send_command(rustpbx::call::domain::CallCommand::QueryLegByDialog {
        dialog_id: c_at_pbx.to_string(), reply,
    }).unwrap();
    let c_leg = tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().unwrap().unwrap();
    assert!(c_leg.as_str().starts_with("transfer-"), "target must keep its own dynamic leg: {c_leg}");

    // Blind inbound REFERs execute INSIDE the original session by default
    // (`inbound_refer_in_session`): Charlie has an independent dynamic leg,
    // and no separate outbound session is created. One logical call stays
    // one session (one CDR).
    sleep(Duration::from_millis(500)).await;
    let calls = server.get_active_calls();
    assert!(
        calls.iter().all(|c| c.direction != "outbound"),
        "in-session REFER must not originate a separate outbound call, got {:?}",
        calls
            .iter()
            .map(|c| (c.direction.clone(), c.callee.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        calls.len(),
        1,
        "the transferred call must remain a single session, got {:?}",
        calls
            .iter()
            .map(|c| (c.direction.clone(), c.callee.clone()))
            .collect::<Vec<_>>()
    );

    let b_at_pbx = rsipstack::dialog::DialogId {
        call_id: bob_dialog.call_id.clone(), local_tag: bob_dialog.remote_tag.clone(), remote_tag: bob_dialog.local_tag.clone(),
    };
    let (reply, result) = tokio::sync::oneshot::channel();
    owner.send_command(rustpbx::call::domain::CallCommand::QueryLegByDialog {
        dialog_id: b_at_pbx.to_string(), reply,
    }).unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().unwrap().unwrap().as_str(), "callee",
        "B must retain its leg and dialog after transfer success until BYE");

    // B's still-live dialog can hold/unhold itself without changing A-C.
    for sdp in [bob_sdp.replace("sendrecv", "sendonly"), bob_sdp.clone()] {
        assert!(bob.send_reinvite(bob_dialog, Some(sdp)).await.unwrap().is_some(),
            "unbridged B must receive its own negotiated SDP answer");
    }

    // INFO from an unpaired participant must not reach the caller.
    alice.process_dialog_events().await.unwrap();
    bob.send_dtmf_info(bob_dialog, "5").await.unwrap();
    assert!(!alice.process_dialog_events().await.unwrap().iter().any(|event|
        matches!(event, TestUaEvent::DtmfInfo(_, _))), "B's DTMF must not reach A after transfer");

    // The dynamically created active callee also owns its incoming requests.
    for sdp in [charlie_reinvite_sdp.replace("sendrecv", "sendonly"), charlie_reinvite_sdp.clone()] {
        assert!(charlie.send_reinvite(c, Some(sdp)).await.unwrap().is_some(),
            "active dynamic C must be able to hold/unhold its selected caller");
    }
    charlie.send_dtmf_info(c, "6").await.unwrap();
    assert!(alice.process_dialog_events().await.unwrap().iter().any(|event|
        matches!(event, TestUaEvent::DtmfInfo(_, digit) if digit == "6")), "C's DTMF must reach A");

    // Transfer success must not make the PBX send BYE to the transferor.
    assert!(!bob.process_dialog_events().await.unwrap().iter().any(|event|
        matches!(event, TestUaEvent::CallTerminated(_))), "PBX must await transferor BYE");
    if transferor_bye {
    bob.hangup(bob_dialog_id.as_ref().unwrap()).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    assert_eq!(server.get_active_calls().len(), 1, "transferor BYE must preserve A-C");
    let (reply, result) = tokio::sync::oneshot::channel();
    owner.send_command(rustpbx::call::domain::CallCommand::QueryLegByDialog {
        dialog_id: b_at_pbx.to_string(), reply,
    }).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().unwrap().is_none(),
        "B's leg/dialog association must be removed on BYE");
    for ua in [&alice, &charlie] {
        assert!(!ua.process_dialog_events().await.unwrap().iter().any(|event|
            matches!(event, TestUaEvent::CallTerminated(_))), "A-C must survive transferor BYE");
    }

    }

    charlie.hangup(charlie_dialog_id.as_ref().unwrap()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if alice.process_dialog_events().await.unwrap().iter().any(|e|
                matches!(e, TestUaEvent::CallTerminated(_))) { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("C hangup must end the transferred call");
    server.stop();
    }
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
/// SESSION even with `route_originated_calls` disabled, instead of originating a raw
/// call to the number:
///
/// 1. Alice calls Bob; Bob answers.
/// 2. Bob sends REFER to `sip:8000@pbx` where 8000 routes to `refer_queue`.
/// 3. PBX replies 202, dispatches the in-session queue hand-off; the REFER
///    subscription still gets its final NOTIFY.
/// 4. Final success precedes the agent answer; Bob sends BYE and A stays queued.
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

    for with_gateway in [true, false] {
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
        // Anchored media: the in-session queue hand-off requires the caller
        // leg to live on a MediaBridge (hold music / agent bridging), which
        // only exists when the call is media-anchored. Under the default
        // `Auto` mode a plain alice→bob call is P2P and the queue dial
        // correctly refuses to run without a bridge.
        media_proxy: MediaProxyMode::All,
        ..Default::default()
    };
    config.route_originated_calls = false;
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
                queue_enricher: None,
                users: vec![
                    SipUser {
                        id: 1,
                        username: "alice".to_string(),
                        // Must match the credentials `create_ua` registers
                        // with (alice → password123, bob → password456,
                        // everyone else → password).
                        password: Some("password123".to_string()),
                        enabled: true,
                        realm: Some("127.0.0.1".to_string()),
                        ..Default::default()
                    },
                    SipUser {
                        id: 2,
                        username: "bob".to_string(),
                        password: Some("password456".to_string()),
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
                rwi_gateway: with_gateway.then_some(gateway),
                #[cfg(feature = "addon-cc")]
                cc_policy_db: None,
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
    let (allow_answer, answer_allowed) = tokio::sync::oneshot::channel();
    let agent_clone = agent.clone();
    let mut answer_allowed = Some(answer_allowed);
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
                    answer_allowed.take().unwrap().await.unwrap();
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

    // The transferor hands A to the queue; success is queue acceptance,
    // not an eventual agent answer.
    let refer_target = format!("sip:{}@{}", REFER_NUMBER, server.proxy_addr);
    let refer_status = bob.send_refer(bob_dialog_id.as_ref().unwrap(), &refer_target)
        .await.expect("send_refer failed");
    assert_eq!(refer_status, 202);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for event in bob.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(_, body, state) if state.starts_with("terminated") => {
                        assert!(body.starts_with("SIP/2.0 200"), "{body}");
                        return;
                    }
                    TestUaEvent::CallTerminated(_) => panic!("queue handoff must await transferor BYE"),
                    _ => {}
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("queue handoff must report success before agent answers");
    sleep(Duration::from_millis(150)).await;
    assert!(!bob.process_dialog_events().await.unwrap().iter().any(|e|
        matches!(e, TestUaEvent::CallTerminated(_))));
    bob.hangup(bob_dialog_id.as_ref().unwrap()).await.unwrap();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(server.get_active_calls().len(), 1, "B BYE must leave A in the queue");
    assert!(!alice.process_dialog_events().await.unwrap().iter().any(|e|
        matches!(e, TestUaEvent::CallTerminated(_))));
    allow_answer.send(()).unwrap();

    // The queue must dial its member
    let agent_dialog_id = tokio::time::timeout(Duration::from_secs(10), agent_answer_handle)
        .await
        .expect("agent answer timeout")
        .expect("agent answer task failed");
    assert!(
        agent_dialog_id.is_some(),
        "Queue member (agent) should receive the transferred call"
    );

    // The in-session hand-off emits call_transferred annotated with the
    // routed target type, and queue_joined proves the QueueApp started in
    // the original session. Webhook payloads nest the event fields under
    // the envelope's `event` object.
    if with_gateway {
    let transferred = wait_webhook_event(&capture, "call_transferred", Duration::from_secs(5))
        .await
        .expect("webhook must receive call_transferred for the REFER queue hand-off");
    assert_eq!(transferred["event"]["transfer_target_type"], "queue");
    assert!(
        transferred["event"]["transfer_target"]
            .as_str()
            .is_some_and(|t| t.contains(REFER_NUMBER)),
        "transfer_target must retain the original bare number: {}",
        transferred["event"]["transfer_target"]
    );

    let joined = wait_webhook_event(&capture, "queue_joined", Duration::from_secs(5))
        .await
        .expect("webhook must receive queue_joined for the in-session queue start");
    assert_eq!(joined["event"]["queue_id"], QUEUE_NAME);
    }

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
}

/// A rejected REFER target must either leave a resumable original call or
/// release the caller if the transferor already left. Exercise real SIP and RTP.
#[tokio::test]
async fn test_inbound_refer_rejection_recovery_and_transferor_bye() {
    use crate::common::rtp_utils::{extract_media_endpoint, RtpPacket};
    use tokio::time::timeout;

    for (transferor_leaves, target_accepts) in [(false, false), (true, false), (true, true)] {
        let mut config = crate::common::test_helpers::test_proxy_config(portpicker::pick_unused_port().unwrap());
        config.media_proxy = MediaProxyMode::All;
        let mut users = crate::common::test_helpers::standard_test_users();
        for user in &mut users { user.is_support_webrtc = false; }
        let gateway = rustpbx::rwi::gateway::RwiGateway::new();
        let mut rwi_events = gateway.subscribe_events();
        let server = E2eTestServer::start_with_inject(config, E2eTestServerInject {
            users, rwi_gateway: Some(Arc::new(parking_lot::RwLock::new(gateway))), ..Default::default()
        }).await.unwrap();
        let alice = server.create_ua("alice").await.unwrap();
        let bob = server.create_ua("bob").await.unwrap();
        let charlie = server.create_ua("charlie").await.unwrap();
        let caller_rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target_rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let agent_rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let caller_sdp = format!("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n", caller_rtp.local_addr().unwrap().port());
        let agent_sdp = caller_sdp.replace(&format!("m=audio {}", caller_rtp.local_addr().unwrap().port()),
            &format!("m=audio {}", agent_rtp.local_addr().unwrap().port()));
        let call = tokio::spawn({
            let alice = alice.clone();
            let sdp = caller_sdp.clone();
            async move { alice.make_call("bob", Some(sdp)).await.unwrap() }
        });
        let (bob_dialog, pbx_agent_rtp) = timeout(Duration::from_secs(5), async {
            loop {
                for event in bob.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, Some(offer)) = event {
                        bob.answer_call(&id, Some(agent_sdp.clone())).await.unwrap();
                        return (id, extract_media_endpoint(&offer).unwrap());
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let caller_dialog = timeout(Duration::from_secs(5), call).await.unwrap().unwrap();
        let answer = alice.get_negotiated_answer_sdp(&caller_dialog).await.unwrap();
        let pbx_caller_rtp = extract_media_endpoint(&answer).unwrap();
        // Match the Yealink: put the customer on hold before REFER.
        bob.send_reinvite(&bob_dialog, Some(agent_sdp.replace("sendrecv", "sendonly"))).await.unwrap();
        let target = format!("sip:charlie@127.0.0.1:{}", charlie.local_port());
        assert_eq!(bob.send_refer(&bob_dialog, &target).await.unwrap(), 202);
        let (target_dialog, pbx_target_rtp) = timeout(Duration::from_secs(5), async {
            loop {
                for event in charlie.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, Some(offer)) = event { return (id, extract_media_endpoint(&offer).unwrap()); }
                }
                sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        charlie.ring_call(&target_dialog).await.unwrap();
        sleep(Duration::from_millis(100)).await;
        let progress = bob.process_dialog_events().await.unwrap();
        assert!(progress.iter().any(|event| matches!(event, TestUaEvent::ReferNotify(_, body, _) if body.contains("100 Trying"))));
        assert!(!progress.iter().any(|event| matches!(event, TestUaEvent::ReferNotify(_, _, state) if state.starts_with("terminated"))),
            "must not report a final outcome while the target is ringing: {progress:?}");
        let owner = server.server_ref.active_call_registry.get_handle_by_dialog(&bob_dialog.call_id).unwrap();
        assert_eq!(owner.snapshot().unwrap().leg_count, 3, "A, B and C must coexist while C rings");
        while let Ok(entry) = rwi_events.try_recv() {
            assert_ne!(entry.event.event_type, "call_transferred", "ringing is not transfer success");
        }
        if transferor_leaves {
            bob.hangup(&bob_dialog).await.unwrap();
            // Give the session loop time to process the original agent's BYE.
            sleep(Duration::from_millis(100)).await;
        }
        if target_accepts {
            let target_sdp = caller_sdp.replace(&format!("m=audio {}", caller_rtp.local_addr().unwrap().port()),
                &format!("m=audio {}", target_rtp.local_addr().unwrap().port()));
            charlie.answer_call(&target_dialog, Some(target_sdp)).await.unwrap();
            let sender = tokio::spawn({
                let a = caller_rtp.clone();
                let c = target_rtp.clone();
                async move {
                    for seq in 0..150u16 {
                        a.send_to(&RtpPacket::new(0, seq, u32::from(seq) * 160, 1234, vec![0x33; 160]).encode(), pbx_caller_rtp).await.unwrap();
                        c.send_to(&RtpPacket::new(0, seq, u32::from(seq) * 160, 9876, vec![0x77; 160]).encode(), pbx_target_rtp).await.unwrap();
                        sleep(Duration::from_millis(20)).await;
                    }
                }
            });
            for (socket, expected) in [(&caller_rtp, 0x77), (&target_rtp, 0x33)] {
                timeout(Duration::from_secs(4), async {
                    let mut buffer = [0u8; 2048];
                    loop {
                        let (len, _) = socket.recv_from(&mut buffer).await.unwrap();
                        if RtpPacket::decode(&buffer[..len]).is_ok_and(|p| p.payload == vec![expected; 160]) { break; }
                    }
                }).await.expect("A-C audio must connect even when B left before C answered");
            }
            sender.abort();
            assert_eq!(server.get_active_calls().len(), 1);
            let mut transferred = 0;
            while let Ok(entry) = rwi_events.try_recv() {
                if entry.event.event_type == "call_transferred" { transferred += 1; }
            }
            assert_eq!(transferred, 1, "successful blind REFER emits exactly once");
            alice.hangup(&caller_dialog).await.unwrap();
            server.stop();
            continue;
        }
        charlie.reject_call_with_reason(&target_dialog, Some(603), None).await.unwrap();
        if transferor_leaves {
            timeout(Duration::from_secs(5), async {
                loop {
                    if alice.process_dialog_events().await.unwrap().iter()
                        .any(|event| matches!(event, TestUaEvent::CallTerminated(id) if id == &caller_dialog)) {
                        break;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("failed transfer after agent BYE must hang up caller without a fallback");
        } else {
            timeout(Duration::from_secs(5), async {
                loop {
                    for event in bob.process_dialog_events().await.unwrap() {
                        match event {
                            TestUaEvent::ReferNotify(_, body, state) if state.starts_with("terminated") => {
                                assert!(body.starts_with("SIP/2.0 603"), "{body}");
                                return;
                            }
                            TestUaEvent::CallTerminated(_) => panic!("original agent must survive rejection"),
                            _ => {}
                        }
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("failure must reach transferor via NOTIFY");
            bob.send_reinvite(&bob_dialog, Some(agent_sdp.clone())).await.unwrap();
            assert_eq!(server.get_active_calls().len(), 1);
            // Both original media endpoints must still work after rejection
            // and unhold, without negotiating replacement RTP ports.
            let sender = tokio::spawn({
                let a = caller_rtp.clone();
                let b = agent_rtp.clone();
                async move {
                    for seq in 0..100u16 {
                        a.send_to(&RtpPacket::new(0, seq, u32::from(seq) * 160, 1234, vec![0x33; 160]).encode(), pbx_caller_rtp).await.unwrap();
                        b.send_to(&RtpPacket::new(0, seq, u32::from(seq) * 160, 5678, vec![0x55; 160]).encode(), pbx_agent_rtp).await.unwrap();
                        sleep(Duration::from_millis(20)).await;
                    }
                }
            });
            for (socket, expected) in [(&caller_rtp, 0x55), (&agent_rtp, 0x33)] {
                timeout(Duration::from_secs(4), async {
                    let mut buffer = [0u8; 2048];
                    loop {
                        let (len, _) = socket.recv_from(&mut buffer).await.unwrap();
                        if RtpPacket::decode(&buffer[..len]).is_ok_and(|p| p.payload == vec![expected; 160]) { break; }
                    }
                }).await.expect("original RTP peer must survive failed transfer");
            }
            sender.abort();
            alice.hangup(&caller_dialog).await.unwrap();
        }
        while let Ok(entry) = rwi_events.try_recv() {
            assert_ne!(entry.event.event_type, "call_transferred", "rejected target is not transfer success");
        }
        server.stop();
    }
}

#[tokio::test]
async fn test_inbound_refer_to_ivr_route() {
    use crate::common::rtp_utils::{extract_media_endpoint, RtpPacket};
    let mut config = crate::common::test_helpers::test_proxy_config(portpicker::pick_unused_port().unwrap());
    config.media_proxy = MediaProxyMode::All;
    config.route_originated_calls = false;
    let ivr = tempfile::NamedTempFile::with_suffix(".toml").unwrap();
    std::fs::write(ivr.path(), r#"
[ivr]
name = "refer_test"
ivr_mode = "tree"
[ivr.root]
greeting = "fixtures/sample.wav"
timeout_ms = 60000
max_retries = 10
timeout_action = { type = "repeat" }
[[ivr.root.entries]]
key = "2"
action = { type = "transfer", target = "charlie" }
"#).unwrap();
    config.routes = Some(vec![RouteRule {
        name: "refer_ivr".to_string(),
        match_conditions: MatchConditions { to_user: Some("888".to_string()), ..Default::default() },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({"file": ivr.path().to_str().unwrap()})),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject {
        users, ..Default::default()
    }).await.unwrap());
    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let bob = server.create_ua("bob").await.expect("create bob failed");

    let charlie = server.create_ua("charlie").await.unwrap();
    let caller_rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let target_rtp = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let alice_sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 12345 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .replace("m=audio 12345", &format!("m=audio {}", caller_rtp.local_addr().unwrap().port()));

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

    // The caller must answer the re-INVITE when a bypass call is anchored
    // at the PBX for the new dynamic target.
    alice.set_answer_sdp(&alice_dialog_id, &alice_sdp).await;

    // Wait for active call in registry
    server
        .wait_for_active_call(Duration::from_secs(3))
        .await
        .expect("Call should be in registry");


    let owner = server.server_ref.active_call_registry.get_handle_by_dialog(&bob_dialog_id.as_ref().unwrap().call_id).unwrap();
    bob.send_reinvite(bob_dialog_id.as_ref().unwrap(), Some(bob_sdp.replace("sendrecv", "sendonly"))).await.unwrap();
    let original = server.get_active_calls()[0].session_id.clone();
    let target = format!("sip:888@{}", server.proxy_addr);
    assert_eq!(bob.send_refer(bob_dialog_id.as_ref().unwrap(), &target).await.unwrap(), 202);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for event in bob.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(_, body, state) if state.starts_with("terminated") => {
                        assert!(body.starts_with("SIP/2.0 200"), "{body}");
                        return;
                    }
                    TestUaEvent::CallTerminated(_) => panic!("IVR handoff must await B BYE"),
                    _ => {}
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("IVR handoff final NOTIFY");
    bob.hangup(bob_dialog_id.as_ref().unwrap()).await.unwrap();
    sleep(Duration::from_millis(200)).await;
    let calls = server.get_active_calls();
    assert_eq!(calls.len(), 1, "IVR must stay in the original session");
    assert_eq!(calls[0].session_id, original);
    assert!(!alice.process_dialog_events().await.unwrap().iter().any(|event|
        matches!(event, TestUaEvent::CallTerminated(_))));
    assert_eq!(owner.snapshot().unwrap().leg_count, 1, "IVR needs only the caller leg");
    let mut buffer = [0u8; 2048];
    tokio::time::timeout(Duration::from_secs(3), caller_rtp.recv_from(&mut buffer))
        .await.expect("IVR playback must reach the lone caller").unwrap();
    alice.send_dtmf_info(&alice_dialog_id, "2").await.unwrap();
    let (target_dialog, pbx_target_rtp) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for event in charlie.process_dialog_events().await.unwrap() {
                if let TestUaEvent::IncomingCall(id, Some(offer)) = event {
                    let answer = alice_sdp.replace(&format!("m=audio {}", caller_rtp.local_addr().unwrap().port()),
                        &format!("m=audio {}", target_rtp.local_addr().unwrap().port()));
                    charlie.answer_call(&id, Some(answer)).await.unwrap();
                    return (id, extract_media_endpoint(&offer).unwrap());
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("IVR must dial the new phone");
    let answer = alice.get_negotiated_answer_sdp(&alice_dialog_id).await.unwrap();
    let pbx_caller_rtp = extract_media_endpoint(&answer).unwrap();
    let sender = tokio::spawn({
        let a = caller_rtp.clone();
        let c = target_rtp.clone();
        async move {
            for seq in 0..150u16 {
                a.send_to(&RtpPacket::new(0, seq, u32::from(seq) * 160, 1234, vec![0x33; 160]).encode(), pbx_caller_rtp).await.unwrap();
                c.send_to(&RtpPacket::new(0, seq, u32::from(seq) * 160, 9876, vec![0x77; 160]).encode(), pbx_target_rtp).await.unwrap();
                sleep(Duration::from_millis(20)).await;
            }
        }
    });
    for (socket, expected) in [(&caller_rtp, 0x77), (&target_rtp, 0x33)] {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let (len, _) = socket.recv_from(&mut buffer).await.unwrap();
                if RtpPacket::decode(&buffer[..len]).is_ok_and(|p| p.payload == vec![expected; 160]) { break; }
            }
        }).await.expect("IVR successor must bridge bidirectional audio");
    }
    sender.abort();
    assert_eq!(owner.snapshot().unwrap().leg_count, 2);
    charlie.hangup(&target_dialog).await.ok();
    alice.hangup(&alice_dialog_id).await.ok();
}

#[tokio::test]
async fn test_inbound_refer_known_unregistered_user() {
    let mut config = crate::common::test_helpers::test_proxy_config(portpicker::pick_unused_port().unwrap());
    config.media_proxy = MediaProxyMode::All;
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject {
        users, ..Default::default()
    }).await.unwrap());
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

    // The caller must answer the re-INVITE when a bypass call is anchored
    // at the PBX for the new dynamic target.
    alice.set_answer_sdp(&alice_dialog_id, &alice_sdp).await;

    // Wait for active call in registry
    server
        .wait_for_active_call(Duration::from_secs(3))
        .await
        .expect("Call should be in registry");


    // Charlie exists in the user backend, but no Charlie UA registers.
    bob.send_reinvite(bob_dialog_id.as_ref().unwrap(), Some(bob_sdp.replace("sendrecv", "sendonly"))).await.unwrap();
    let target = format!("sip:charlie@{}", server.proxy_addr);
    assert_eq!(bob.send_refer(bob_dialog_id.as_ref().unwrap(), &target).await.unwrap(), 202);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            for event in bob.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(_, body, state) if state.starts_with("terminated") => {
                        assert!(body.starts_with("SIP/2.0 480"), "{body}");
                        return;
                    }
                    TestUaEvent::CallTerminated(_) => panic!("Offline target must preserve transferor dialog"),
                    _ => {}
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let owner = server.server_ref.active_call_registry.get_handle_by_dialog(&bob_dialog_id.as_ref().unwrap().call_id).unwrap();
    assert_eq!(owner.snapshot().unwrap().leg_count, 2);
    assert_eq!(server.get_active_calls().len(), 1);
    bob.send_reinvite(bob_dialog_id.as_ref().unwrap(), Some(bob_sdp)).await.unwrap();
    assert!(!alice.process_dialog_events().await.unwrap().iter().any(|event|
        matches!(event, TestUaEvent::CallTerminated(_))));
    alice.hangup(&alice_dialog_id).await.ok();
}
