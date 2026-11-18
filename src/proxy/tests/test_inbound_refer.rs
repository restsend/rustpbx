//! Inbound REFER configuration. Real subscription/lifecycle coverage lives in proxy_e2e.

use crate::config::ProxyConfig;

#[test]
fn inbound_refer_in_session_defaults_enabled() {
    let config = ProxyConfig::default();
    assert!(
        config.inbound_refer_in_session,
        "blind inbound REFERs must execute in-session by default"
    );
}

/// Queue dispatch uses a dynamic agent leg; attended REFER must preserve
/// the existing customer and consultation dialogs through the handoff.
#[tokio::test]
async fn attended_refer_from_queue_agent_preserves_existing_dialogs() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    use super::e2e_test_server::E2eTestServer;
    use super::test_ua::TestUaEvent;
    use crate::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    let mut config = ProxyConfig::default();
    config.media_proxy = crate::config::MediaProxyMode::All;

    // The transfer-room fallback path: opt out of the REFER-Replaces relay.
    config.refer_replaces_relay = false;
    config.queues.insert(
        "support".into(),
        RouteQueueConfig {
            name: Some("support".into()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "sip:bob@127.0.0.1".into(),
                    label: None,
                }],
                ..Default::default()
            },
            accept_immediately: false,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "support".into(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".into()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".into()),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let server = E2eTestServer::start_with_config(config).await.unwrap();
    for mut user in super::test_helpers::standard_test_users() {
        user.is_support_webrtc = false;
        server
            .server_ref
            .user_backend
            .create_user(user)
            .await
            .unwrap();
    }
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut calls = Vec::new();
    for (caller, callee, target) in [(&alice, &bob, "support"), (&bob, &charlie, "charlie")] {
        let caller = caller.clone();
        let offer = sdp.clone();
        let dial =
            crate::utils::spawn(
                async move { caller.make_call(target, Some(offer)).await.unwrap() },
            );
        let incoming = timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        calls.push((dial.await.unwrap(), incoming));
    }
    bob.send_reinvite(&calls[0].1, Some(sdp.replace("sendrecv", "sendonly")))
        .await
        .unwrap();
    let consult = &calls[1].0;
    let replaces = format!(
        "{};to-tag={};from-tag={}",
        consult.call_id, consult.remote_tag, consult.local_tag
    );
    let target = format!(
        "sip:charlie@{}?Replaces={}",
        server.proxy_addr,
        urlencoding::encode(&replaces)
    );
    assert_eq!(bob.send_refer(&calls[0].1, &target).await.unwrap(), 202);
    timeout(Duration::from_secs(8), async {
        loop {
            for ua in [&alice, &bob, &charlie] {
                ua.process_dialog_events().await.unwrap();
            }
            if let Some(room) = server
                .server_ref
                .conference_server
                .list_conferences_detail()
                .await
                .into_iter()
                .find(|room| room.participant_count() == 2)
            {
                assert!(room.focus_uri.is_none());
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("queue agent REFER must join the surviving dialogs");
    bob.hangup(&calls[0].1).await.unwrap();
    bob.hangup(&calls[1].0).await.unwrap();
    assert_eq!(server.registry.count(), 2);
    alice.hangup(&calls[0].0).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            // Keep every UA polled: the final attended-REFER NOTIFY is still
            // in flight to the transferor when the room first appears, and an
            // unpolled test UA would let it time out (5 s) instead of ACKing.
            for ua in [&alice, &bob, &charlie] {
                ua.process_dialog_events().await.unwrap();
            }
            if server.registry.count() == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("customer departure must release both transferred sessions");
    server.stop();
}

/// Attended transfer then the transfer TARGET holds: the held party (the
/// customer) must receive the hold re-INVITE routed through the partner
/// session. After the transferor departs, the in-session bridged peer is the
/// retired consult dialog — without the cross-session partner routing the
/// hold never reaches the customer (regression: no hold music, then silence +
/// garbage after resume, prd 2026-11-12 audio_f4).
#[tokio::test]
async fn attended_transfer_hold_reaches_customer_via_partner_session() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    use super::e2e_test_server::E2eTestServer;
    use super::test_ua::TestUaEvent;
    use crate::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    let mut config = ProxyConfig::default();
    config.media_proxy = crate::config::MediaProxyMode::All;

    // The transfer-room fallback path: opt out of the REFER-Replaces relay.
    config.refer_replaces_relay = false;
    config.queues.insert(
        "support".into(),
        RouteQueueConfig {
            name: Some("support".into()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "sip:bob@127.0.0.1".into(),
                    label: None,
                }],
                ..Default::default()
            },
            accept_immediately: false,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "support".into(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".into()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".into()),
            ..Default::default()
        },
        ..Default::default()
    }]);
    let server = E2eTestServer::start_with_config(config).await.unwrap();
    for mut user in super::test_helpers::standard_test_users() {
        user.is_support_webrtc = false;
        server
            .server_ref
            .user_backend
            .create_user(user)
            .await
            .unwrap();
    }
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut calls = Vec::new();
    for (caller, callee, target) in [(&alice, &bob, "support"), (&bob, &charlie, "charlie")] {
        let caller = caller.clone();
        let offer = sdp.clone();
        let dial =
            crate::utils::spawn(
                async move { caller.make_call(target, Some(offer)).await.unwrap() },
            );
        let incoming = timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        calls.push((dial.await.unwrap(), incoming));
    }
    bob.send_reinvite(&calls[0].1, Some(sdp.replace("sendrecv", "sendonly")))
        .await
        .unwrap();
    let consult = &calls[1].0;
    let replaces = format!(
        "{};to-tag={};from-tag={}",
        consult.call_id, consult.remote_tag, consult.local_tag
    );
    let target = format!(
        "sip:charlie@{}?Replaces={}",
        server.proxy_addr,
        urlencoding::encode(&replaces)
    );
    assert_eq!(bob.send_refer(&calls[0].1, &target).await.unwrap(), 202);
    timeout(Duration::from_secs(8), async {
        loop {
            for ua in [&alice, &bob, &charlie] {
                ua.process_dialog_events().await.unwrap();
            }
            if let Some(room) = server
                .server_ref
                .conference_server
                .list_conferences_detail()
                .await
                .into_iter()
                .find(|room| room.participant_count() == 2)
            {
                assert!(room.focus_uri.is_none());
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("queue agent REFER must join the surviving dialogs");

    // Transferor departs: BYE the consult dialog and the customer-side
    // dialog (the client exits the call entirely after completing a
    // transfer). The room keeps the customer and the transfer target.
    bob.hangup(&calls[1].0).await.unwrap();
    bob.hangup(&calls[0].1).await.unwrap();
    sleep(Duration::from_millis(300));
    for ua in [&alice, &charlie] {
        ua.process_dialog_events().await.unwrap();
    }

    // The transfer target holds: the hold re-INVITE must reach the CUSTOMER
    // (sendonly) via the cross-session partner routing.
    charlie
        .send_reinvite(&calls[1].1, Some(sdp.replace("sendrecv", "sendonly")))
        .await
        .unwrap();
    let alice_dialog = calls[0].0.clone();
    timeout(Duration::from_secs(8), async {
        loop {
            for event in alice.process_dialog_events().await.unwrap() {
                if let TestUaEvent::CallUpdated(id, method, Some(body)) = event {
                    if id == alice_dialog
                        && method == rsipstack::sip::Method::Invite
                        && body.contains("a=sendonly")
                    {
                        return true;
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("hold re-INVITE must reach the customer via the partner session");

    // Unhold: the customer must be restored (sendrecv re-INVITE).
    charlie.send_reinvite(&calls[1].1, Some(sdp.clone())).await.unwrap();
    timeout(Duration::from_secs(8), async {
        loop {
            for event in alice.process_dialog_events().await.unwrap() {
                if let TestUaEvent::CallUpdated(id, method, Some(body)) = event {
                    if id == alice_dialog
                        && method == rsipstack::sip::Method::Invite
                        && body.contains("a=sendrecv")
                    {
                        return true;
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("unhold re-INVITE must reach the customer via the partner session");

    // Cleanup: target hangs up — the customer call must release cleanly.
    charlie.hangup(&calls[1].1).await.unwrap();
    timeout(Duration::from_secs(6), async {
        loop {
            alice.process_dialog_events().await.unwrap();
            if server.registry.count() == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("target departure must release the transferred customer call");
    server.stop();
}

/// Attended transfer via the REFER-Replaces chain (no transfer room): the
/// transferor's REFER is RELAYED to the customer's UA, which executes the
/// standard transferee role — INVITE(Replaces=<consult dialog>) — and the
/// replacement attaches as a normal in-session bridge on this node. Asserts
/// the whole chain: the REFER reaches the customer, the replacement INVITE
/// carries the Replaces header, no transfer room is created, the replaced
/// (consult) session releases, and A↔C stay in call.
#[tokio::test]
async fn attended_transfer_relayed_via_refer_replaces_without_room() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    use super::e2e_test_server::E2eTestServer;
    use super::test_ua::TestUaEvent;
    use crate::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    let mut config = ProxyConfig::default();
    config.media_proxy = crate::config::MediaProxyMode::All;
    config.queues.insert(
        "support".into(),
        RouteQueueConfig {
            name: Some("support".into()),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: "sip:bob@127.0.0.1".into(),
                    label: None,
                }],
                ..Default::default()
            },
            accept_immediately: false,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "support".into(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".into()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".into()),
            ..Default::default()
        },
        ..Default::default()
    }]);
        // The customer arrives via a Replaces-capable trunk (refer_replaces
    // defaults to true) — the REFER-Replaces relay path engages.
    config.trunks.insert(
        "pstn_test".to_string(),
        crate::proxy::routing::TrunkConfig {
            dest: "sip:pstn.example.com:5060".to_string(),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        },
    );
let server = E2eTestServer::start_with_config(config).await.unwrap();
    for mut user in super::test_helpers::standard_test_users() {
        user.is_support_webrtc = false;
        server
            .server_ref
            .user_backend
            .create_user(user)
            .await
            .unwrap();
    }
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut calls = Vec::new();
    for (caller, callee, target) in [(&alice, &bob, "support"), (&bob, &charlie, "charlie")] {
        let caller = caller.clone();
        let offer = sdp.clone();
        let dial =
            crate::utils::spawn(
                async move { caller.make_call(target, Some(offer)).await.unwrap() },
            );
        let incoming = timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        calls.push((dial.await.unwrap(), incoming));
    }
    bob.send_reinvite(&calls[0].1, Some(sdp.replace("sendrecv", "sendonly")))
        .await
        .unwrap();
    let consult = &calls[1].0;
    let replaces = format!(
        "{};to-tag={};from-tag={}",
        consult.call_id, consult.remote_tag, consult.local_tag
    );
    let target = format!(
        "sip:charlie@{}?Replaces={}",
        server.proxy_addr,
        urlencoding::encode(&replaces)
    );
    assert_eq!(bob.send_refer(&calls[0].1, &target).await.unwrap(), 202);

    // The REFER must be RELAYED to the customer's UA (the transferee).
    let referred = timeout(Duration::from_secs(8), async {
        loop {
            for event in alice.process_dialog_events().await.unwrap_or_else(|e| {
                eprintln!("alice process_dialog_events error: {e}");
                Vec::new()
            }) {
                if let TestUaEvent::Referred(_, refer_to) = event {
                    return refer_to;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("REFER must be relayed to the customer UA");
    assert!(
        referred.contains("charlie") && referred.contains("Replaces="),
        "Refer-To must point at the transfer target with Replaces: {referred}"
    );

    // No transfer room may exist (the relay skips the room handshake).
    let rooms = server
        .server_ref
        .conference_server
        .list_conferences_detail()
        .await;
    assert!(
        !rooms.iter().any(|room| room.participant_count() >= 2),
        "REFER-Replaces chain must not create a transfer room"
    );

    // The customer executes the transferee role: INVITE(Replaces) → charlie
    // answers (the event loop runs concurrently with the INVITE); the
    // replacement bridges in a normal session.
    let charlie_replacement = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let charlie_replacement_loop = charlie_replacement.clone();
    let replacement = tokio::select! {
        r = alice.execute_referred(&referred, Some(sdp.clone())) => {
            r.expect("customer replacement call must establish")
        }
        _ = async {
            loop {
                for event in charlie.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        charlie.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        *charlie_replacement_loop.lock().await = Some(id);
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        } => {
            panic!("charlie event loop ended unexpectedly");
        }
    };
    let charlie_replacement_dialog = charlie_replacement
        .lock()
        .await
        .clone()
        .expect("charlie replacement dialog must be captured");
    timeout(Duration::from_secs(8), async {
        loop {
            for event in bob.process_dialog_events().await.unwrap() {
                if let TestUaEvent::CallTerminated(id) = event {
                    if id == calls[1].0 {
                        return true;
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("replaced consult dialog must terminate once the replacement answers");

    // The transferee reports the replacement outcome on the implicit REFER
    // subscription. A STANDARD transferee (baresip) stamps the RFC 3515 `id`
    // parameter: `Event: refer;id=40999`. The server must match the package
    // token, deliver the deferred final NOTIFY to the transferor (bob) and
    // release its original (customer-side) leg.
    alice
        .send_refer_notify(&calls[0].0, 200, "refer;id=40999")
        .await
        .unwrap();

    timeout(Duration::from_secs(8), async {
        let mut notified = false;
        let mut released = false;
        loop {
            for event in bob.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(id, status) => {
                        assert_eq!(id, calls[0].1, "final NOTIFY must ride the REFER dialog");
                        assert_eq!(status, 200, "transferor's final NOTIFY must carry 200");
                        notified = true;
                    }
                    TestUaEvent::CallTerminated(id) if id == calls[0].1 => {
                        released = true;
                    }
                    _ => {}
                }
            }
            if notified && released {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect(
        "transferor must get the final NOTIFY (Event: refer;id=…) and release its original leg",
    );

    // Charlie then hangs up the replacement — everything must release.
    charlie.hangup(&charlie_replacement_dialog).await.unwrap();
    timeout(Duration::from_secs(6), async {
        loop {
            for ua in [&alice, &bob, &charlie] {
                ua.process_dialog_events().await.unwrap();
            }
            if server.registry.count() == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all transferred sessions must release after the replacement ends");
    server.stop();
}
