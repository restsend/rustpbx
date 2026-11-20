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
async fn review_regression_attended_refer_from_queue_agent_preserves_existing_dialogs() {
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
    // A self-owned Replaces must be rejected without parking this actor;
    // the valid attended transfer below must still work on the same call.
    let original = &calls[0].0;
    let self_replaces = format!(
        "{};to-tag={};from-tag={}",
        original.call_id, original.remote_tag, original.local_tag
    );
    let self_target = format!(
        "sip:support@{}?Replaces={}",
        server.proxy_addr,
        urlencoding::encode(&self_replaces)
    );
    assert_eq!(
        bob.send_refer(&calls[0].1, &self_target).await.unwrap(),
        202
    );
    let owner = server
        .registry
        .get_handle_by_dialog(&original.call_id)
        .unwrap();
    let (reply, mut response) = tokio::sync::oneshot::channel();
    owner
        .send_command(crate::call::domain::CallCommand::QueryAnsweredConsultTarget { reply })
        .unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            for ua in [&alice, &bob, &charlie] {
                ua.process_dialog_events().await.unwrap();
            }
            if response.try_recv().is_ok() {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("self Replaces must not block the session command queue");
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
        let mut target_updated = false;
        let mut transfer_succeeded = false;
        loop {
            alice.process_dialog_events().await.unwrap();
            for event in charlie.process_dialog_events().await.unwrap() {
                if matches!(event, TestUaEvent::CallUpdated(ref id, rsipstack::sip::Method::Update, None) if id == &calls[1].1) {
                    target_updated = true;
                }
            }
            for event in bob.process_dialog_events().await.unwrap() {
                if matches!(event, TestUaEvent::ReferNotify(ref id, 200) if id == &calls[0].1) {
                    transfer_succeeded = true;
                }
            }
            if target_updated && transfer_succeeded {
                let rooms = server.server_ref.conference_server.list_conferences_detail().await;
                assert!(rooms.iter().any(|room| room.participant_count() == 2 && room.focus_uri.is_none()));
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("retained target dialog must be notified after the handoff");
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
    assert_attended_transfer_retains_customer_and_target(false).await;
}

#[tokio::test]
async fn attended_transfer_outbound_trunk_retains_customer_and_target() {
    assert_attended_transfer_retains_customer_and_target(true).await;
}

async fn assert_attended_transfer_retains_customer_and_target(outbound: bool) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    use super::e2e_test_server::E2eTestServer;
    use super::rtp_utils::{RtpPacket, extract_media_endpoint};
    use super::test_helpers::pcmu_sdp;
    use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
    use crate::proxy::routing::{
        DestConfig, MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule, TrunkConfig,
    };
    use std::time::Duration;
    use tokio::net::UdpSocket;
    use tokio::time::{sleep, timeout};

    let mut config = ProxyConfig::default();
    config.media_proxy = crate::config::MediaProxyMode::All;

    config.refer_replaces_relay = outbound;
    let customer_port = portpicker::pick_unused_port().unwrap();
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
    if outbound {
        config.trunks.insert(
            "pstn".into(),
            TrunkConfig {
                dest: format!("sip:127.0.0.1:{customer_port}"),
                refer_replaces: true,
                ..Default::default()
            },
        );
        config.routes.as_mut().unwrap().push(RouteRule {
            name: "outbound".into(),
            priority: 20,
            match_conditions: MatchConditions {
                to_user: Some("^18005550100$".into()),
                ..Default::default()
            },
            action: RouteAction {
                action: Some("forward".into()),
                dest: Some(DestConfig::Single("pstn".into())),
                ..Default::default()
            },
            ..Default::default()
        });
    }
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
    let mut alice = TestUa::new(TestUaConfig {
        username: "alice".into(),
        password: "password123".into(),
        realm: "127.0.0.1".into(),
        local_port: customer_port,
        proxy_addr: server.proxy_addr,
        webrtc: false,
        extra_invite_headers: vec![],
    });
    alice.start().await.unwrap();
    if !outbound {
        alice.register().await.unwrap();
    }
    let mut bob = TestUa::new(TestUaConfig {
        username: "bob".into(),
        password: "password456".into(),
        realm: "127.0.0.1".into(),
        local_port: portpicker::pick_unused_port().unwrap(),
        proxy_addr: server.proxy_addr,
        webrtc: false,
        extra_invite_headers: vec![],
    });
    bob.start_with_user_agent(Some("restsend-call/0.1.26"))
        .await
        .unwrap();
    bob.register().await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let customer_media = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target_media = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let customer_sdp = pcmu_sdp("127.0.0.1", customer_media.local_addr().unwrap().port());
    let target_sdp = pcmu_sdp("127.0.0.1", target_media.local_addr().unwrap().port());
    let sdp = pcmu_sdp("127.0.0.1", 23456);
    let mut calls = Vec::new();
    let original = if outbound {
        (&bob, &alice, "18005550100", &sdp, &customer_sdp)
    } else {
        (&alice, &bob, "support", &customer_sdp, &sdp)
    };
    let mut incoming_media = Vec::new();
    for (caller, callee, target, offer, answer) in
        [original, (&bob, &charlie, "charlie", &sdp, &target_sdp)]
    {
        let caller = caller.clone();
        let offer = offer.clone();
        let dial =
            crate::utils::spawn(
                async move { caller.make_call(target, Some(offer)).await.unwrap() },
            );
        let incoming = timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, offer) = event {
                        callee.answer_call(&id, Some(answer.clone())).await.unwrap();
                        incoming_media.push(extract_media_endpoint(&offer.unwrap()).unwrap());
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
    let main_session = server
        .registry
        .get_handle_by_dialog(&calls[0].0.call_id)
        .unwrap()
        .session_id()
        .to_string();
    let consult_session = server
        .registry
        .get_handle_by_dialog(&calls[1].0.call_id)
        .unwrap()
        .session_id()
        .to_string();
    let (source_dialog, customer_dialog) = if outbound {
        (&calls[0].0, &calls[0].1)
    } else {
        (&calls[0].1, &calls[0].0)
    };
    bob.send_reinvite(source_dialog, Some(sdp.replace("sendrecv", "sendonly")))
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
    assert_eq!(bob.send_refer(source_dialog, &target).await.unwrap(), 202);
    timeout(Duration::from_secs(8), async {
        let mut committed = false;
        let mut target_updated = false;
        while !(committed && target_updated) {
            for event in alice.process_dialog_events().await.unwrap() {
                assert!(!matches!(event, TestUaEvent::Referred(..)),
                    "the existing customer dialog must not execute the transfer");
                assert!(!matches!(event, TestUaEvent::CallTerminated(..)),
                    "the existing customer dialog must stay connected");
            }
            for event in charlie.process_dialog_events().await.unwrap() {
                assert!(!matches!(event, TestUaEvent::IncomingCall(..)),
                    "the consulted agent must not ring a second time");
                if matches!(event, TestUaEvent::CallUpdated(ref id, rsipstack::sip::Method::Update, None) if id == &calls[1].1) {
                    target_updated = true;
                }
            }
            for event in bob.process_dialog_events().await.unwrap() {
                if let TestUaEvent::ReferNotify(id, status @ 200..) = event {
                    assert_eq!(&id, source_dialog);
                    assert_eq!(status, 200, "attended transfer must commit");
                    committed = true;
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("attended REFER must connect the existing customer and consulted agent");

    // Transferor departs: BYE the consult dialog and the customer-side
    // dialog (the client exits the call entirely after completing a
    // transfer). The room keeps the customer and the transfer target.
    bob.hangup(&calls[1].0).await.unwrap();
    bob.hangup(source_dialog).await.unwrap();
    sleep(Duration::from_millis(300)).await;
    for ua in [&alice, &charlie] {
        ua.process_dialog_events().await.unwrap();
    }

    if outbound {
        // Two distinct PCMU waveforms prove both directions after A leaves.
        // Sustained positive and negative frames cannot come from the hold tone.
        let send = async {
            for seq in 0..40 {
                for (socket, addr, ssrc, sample) in [
                    (&customer_media, incoming_media[0], 17, 0xa0),
                    (&target_media, incoming_media[1], 29, 0x20),
                ] {
                    let packet =
                        RtpPacket::new(0, seq, u32::from(seq) * 160, ssrc, vec![sample; 160]);
                    socket.send_to(&packet.encode(), addr).await.unwrap();
                }
                sleep(Duration::from_millis(20)).await;
            }
        };
        async fn expect_waveform(socket: &UdpSocket, positive: bool) {
            let mut buf = [0; 1500];
            let mut matching = 0;
            while matching < 8 {
                let len = socket.recv(&mut buf).await.unwrap();
                let Ok(packet) = RtpPacket::decode(&buf[..len]) else {
                    continue;
                };
                if packet.payload_type == 0
                    && packet
                        .payload
                        .iter()
                        .filter(|&&sample| {
                            (sample & 0x80 != 0) == positive && (sample & 0x7f) < 0x70
                        })
                        .count()
                        >= 120
                {
                    matching += 1;
                }
            }
        }
        timeout(Duration::from_secs(3), async {
            tokio::join!(
                send,
                expect_waveform(&customer_media, false),
                expect_waveform(&target_media, true)
            );
        })
        .await
        .expect("customer and B must exchange RTP after A leaves");
    }

    // The transfer target holds: the hold re-INVITE must reach the CUSTOMER
    // (sendonly) via the cross-session partner routing.
    charlie
        .send_reinvite(
            &calls[1].1,
            Some(target_sdp.replace("sendrecv", "sendonly")),
        )
        .await
        .unwrap();
    let alice_dialog = customer_dialog.clone();
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
    charlie
        .send_reinvite(&calls[1].1, Some(target_sdp))
        .await
        .unwrap();
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
    let records = server
        .cdr_capture
        .wait_for_records(2, Duration::from_secs(5))
        .await
        .expect("preserve both call records");
    for call_id in [&main_session, &consult_session] {
        let record = records
            .iter()
            .find(|record| &record.call_id == call_id)
            .unwrap();
        assert_eq!(record.session_id.as_deref(), Some(main_session.as_str()));
    }
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
    assert_refer_replaces_record_ownership(RelayOutcome::Answered).await;
}

#[tokio::test]
async fn rejected_replaces_keeps_the_consult_and_its_cdr() {
    assert_refer_replaces_record_ownership(RelayOutcome::Rejected).await;
}

#[tokio::test]
async fn attended_transfer_silent_relay_reports_failure_and_keeps_the_original_call() {
    assert_refer_replaces_record_ownership(RelayOutcome::Silent).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayOutcome {
    Answered,
    Rejected,
    Silent,
}

async fn assert_refer_replaces_record_ownership(outcome: RelayOutcome) {
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
    // The customer arrives via a Replaces-capable trunk.
    config.trunks.insert(
        "pstn_test".to_string(),
        crate::proxy::routing::TrunkConfig {
            dest: "sip:pstn.example.com:5060".to_string(),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            refer_replaces: true,
            ..Default::default()
        },
    );
    let gateway = std::sync::Arc::new(parking_lot::RwLock::new(crate::rwi::RwiGateway::new()));
    let mut events = gateway.read().subscribe_events();
    let server = E2eTestServer::start_with_config_and_gateway(config, Some(gateway.clone()))
        .await
        .unwrap();
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
    // The consult dialog's target identity carries the *branded realm* of the
    // transferor's registration (prod: a WSS-only domain like
    // `pbx.ruzhila.cn` with no UDP/TCP SIP route). A third-party transferee
    // (baresip / PSTN-style) dials the Refer-To host literally — against a
    // WSS-only realm that INVITE never reaches us and the transfer times out
    // (~15s failure NOTIFY; the 2026-11-19 prd-mac incident). The relay must
    // repoint the target at the node's routable SIP address before handing
    // the REFER to the transferee, so the REFER here deliberately carries a
    // realm the TestUa cannot dial.
    let target = format!(
        "sip:charlie@wss-only-realm.example.com?Replaces={}",
        urlencoding::encode(&replaces)
    );
    let session_id = server
        .registry
        .get_handle_by_dialog(&calls[0].0.call_id)
        .unwrap()
        .session_id()
        .to_string();
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
    assert!(
        referred.contains(&server.proxy_addr.to_string()),
        "Refer-To host must be repointed at the node's routable SIP address \
         (a third-party transferee dials the host literally — an unroutable \
         branded realm kills the transfer): {referred}"
    );
    assert!(
        !referred.contains("wss-only-realm.example.com"),
        "the unroutable branded realm must not survive the relay: {referred}"
    );

    if outcome == RelayOutcome::Silent {
        timeout(Duration::from_secs(38), async {
            loop {
                for event in bob.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::ReferNotify(id, status @ 200..) = event {
                        assert_eq!(id, calls[0].1);
                        assert_eq!(
                            status, 408,
                            "a silent relay must terminate with an explicit failure"
                        );
                        return;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the PBX must send a final failure when the transferee only accepts");
        assert!(server.registry.get_handle(&session_id).is_some());
        assert!(
            server
                .registry
                .get_handle_by_dialog(&calls[1].0.call_id)
                .is_some()
        );
        while let Ok(entry) = events.try_recv() {
            assert!(
                entry.call_id != session_id || entry.event.event_type != "call_transferred",
                "receipt of REFER must not publish a transfer that never completed"
            );
        }
        // The old peer can report success after the PBX deadline. Its stale
        // NOTIFY must never end the call the transferor can still retrieve.
        alice
            .send_refer_notify(&calls[0].0, 200, "refer;id=40999")
            .await
            .unwrap();
        assert!(
            bob.send_reinvite(&calls[0].1, Some(sdp.clone()))
                .await
                .unwrap()
                .is_some()
        );
        for event in bob.process_dialog_events().await.unwrap() {
            assert!(!matches!(event, TestUaEvent::CallTerminated(..)));
        }
        server.stop();
        return;
    }

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

    let old_handle = server
        .registry
        .get_handle_by_dialog(&calls[1].0.call_id)
        .expect("consult handle");
    let old_session_id = old_handle.session_id().to_string();
    if outcome == RelayOutcome::Answered {
        assert_ne!(old_session_id, session_id);
        // A tracked transferor's consultation carries the customer call as
        // its logical root, while the two sessions have separate CDR IDs.
        let gateway = gateway.read();
        let mut consult_meta = gateway.meta_store.get_sync(&old_session_id).unwrap();
        consult_meta.session_id = Some(session_id.clone());
        gateway.meta_store.insert(old_session_id.clone(), consult_meta);
    }
    if outcome == RelayOutcome::Rejected {
        let mut attempt = Box::pin(alice.execute_referred(&referred, Some(sdp.clone())));
        tokio::select! {
            result = &mut attempt => panic!("replacement failed before reaching the target: {result:?}"),
            _ = async {
                loop {
                    for event in charlie.process_dialog_events().await.unwrap() {
                        if let TestUaEvent::IncomingCall(id, _) = event {
                            charlie.reject_call_with_reason(&id, Some(486), None).await.unwrap();
                            return;
                        }
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            } => {}
        }
        let error = timeout(Duration::from_secs(2), attempt)
            .await
            .expect("486 from an internal replacement must bypass the 3-second busy tone")
            .unwrap_err();
        assert!(error.to_string().contains("486"), "{error}");
        assert!(
            server.registry.get_handle(&old_session_id).is_some(),
            "failed replacement must preserve the consult"
        );
        bob.hangup(&calls[1].0).await.unwrap();
        alice.hangup(&calls[0].0).await.unwrap();
        let record = server
            .cdr_capture
            .wait_for_record(&old_session_id, Duration::from_secs(6))
            .await
            .expect("failed replacement must leave the old consult CDR intact");
        assert_eq!(record.call_id, old_session_id);
        server.stop();
        return;
    }

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
    while let Ok(entry) = events.try_recv() {
        assert!(
            entry.call_id != session_id || entry.event.event_type != "call_transferred",
            "relay acceptance must not publish transfer success before the replacement outcome"
        );
    }
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

    let mut completed_events = 0;
    while let Ok(entry) = events.try_recv() {
        if entry.call_id == session_id && entry.event.event_type == "call_transferred" {
            completed_events += 1;
        }
    }
    assert_eq!(
        completed_events, 1,
        "publish transfer completion exactly once"
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
    let records = server
        .cdr_capture
        .wait_for_records(2, Duration::from_secs(5))
        .await
        .expect("original and replacement CDRs");
    assert_eq!(
        records
            .iter()
            .filter(|record| record.call_id == old_session_id)
            .count(),
        1,
        "the replaced consult and its successor must not both publish the inherited record"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.call_id == session_id)
            .count(),
        1,
        "the original customer session must keep its own CDR ID"
    );
    let replacement_record = records
        .iter()
        .find(|record| record.call_id == old_session_id)
        .unwrap();
    assert_eq!(
        replacement_record.session_id.as_deref(),
        Some(session_id.as_str())
    );
    server.stop();
}

#[cfg(feature = "commerce")]
#[tokio::test]
async fn attended_transfer_resolves_consult_owner_before_registered_target() {
    use super::test_helpers::{pcmu_sdp, standard_test_users, test_proxy_config};
    use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
    use crate::app::AppStateBuilder;
    use crate::config::{AmiConfig, ClusterConfig, ClusterPeer, Config, LocatorConfig, MediaProxyMode};
    use rsipstack::dialog::DialogId;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::time::{sleep, timeout};

    let root = tempfile::tempdir().unwrap();
    let db = |name| format!("sqlite://{}?mode=rwc", root.path().join(name).display());
    let mut listeners = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        peers.push(ClusterPeer {
            addr: "127.0.0.1".into(),
            sip_port: portpicker::pick_unused_port().unwrap(),
            ami_port: listener.local_addr().unwrap().port(),
        });
        listeners.push(listener);
    }
    let cluster = ClusterConfig {
        peers: peers.clone(),
        session_registry_backend: "db".into(),
        ..Default::default()
    };
    let mut tasks = tokio::task::JoinSet::new();
    let mut nodes = Vec::new();
    for (peer, listener) in peers.iter().zip(listeners) {
        let mut proxy = test_proxy_config(peer.sip_port);
        proxy.ensure_user = Some(false);
        proxy.locator = LocatorConfig::Database { url: db("registrations.sqlite") };
        proxy.media_proxy = MediaProxyMode::All;
        proxy.trunks.insert(
            "local_transferee".into(),
            crate::proxy::routing::TrunkConfig {
                dest: "sip:trunk.example.test:5060".into(),
                inbound_hosts: vec!["127.0.0.1".into()],
                ..Default::default()
            },
        );
        let app = AppStateBuilder::new()
            .with_config(Config {
                database_url: db("sessions.sqlite"),
                proxy,
                cluster: Some(cluster.clone()),
                ami: Some(AmiConfig::default()),
                ..Default::default()
            })
            .build().await.unwrap();
        for mut user in standard_test_users() {
            user.is_support_webrtc = false;
            app.sip_server().get_inner().user_backend.create_user(user).await.unwrap();
        }
        let router = crate::handler::ami_router(app.clone()).with_state(app.clone());
        tasks.spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
                .await.unwrap();
        });
        nodes.push(app.clone());
        tasks.spawn(async move { app.sip_server().serve().await.unwrap() });
    }

    let mut phones = Vec::new();
    for (user, peer) in standard_test_users().into_iter().zip(&peers) {
        let mut ua = TestUa::new(TestUaConfig {
            username: user.username,
            password: user.password.unwrap(),
            realm: "127.0.0.1".into(),
            local_port: portpicker::pick_unused_port().unwrap(),
            proxy_addr: format!("127.0.0.1:{}", peer.sip_port).parse().unwrap(),
            webrtc: false,
            extra_invite_headers: vec![],
        });
        ua.start().await.unwrap();
        ua.register().await.unwrap();
        phones.push(ua);
    }
    let [customer, transferor, target] = phones.as_slice() else { unreachable!() };

    async fn next_invite(ua: &TestUa) -> DialogId {
        loop {
            for event in ua.process_dialog_events().await.unwrap() {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    return id;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    }

    let sdp = pcmu_sdp("127.0.0.1", 23456);
    let (original, customer_leg) = timeout(Duration::from_secs(5), async {
        tokio::join!(customer.make_call("bob", Some(sdp.clone())), async {
            let id = next_invite(transferor).await;
            transferor.answer_call(&id, Some(sdp.clone())).await.unwrap();
            id
        })
    }).await.unwrap();
    let original = original.unwrap();
    let (consultation, target_leg) = timeout(Duration::from_secs(5), async {
        tokio::join!(transferor.make_call("charlie", Some(sdp.clone())), async {
            let id = next_invite(target).await;
            target.answer_call(&id, Some(sdp.clone())).await.unwrap();
            id
        })
    }).await.unwrap();
    let consultation = consultation.unwrap();
    assert!(nodes[1].sip_server().get_inner().active_call_registry
        .get_handle_by_dialog(&consultation.call_id).is_some());

    // A second device registering after Charlie answered changes the default
    // dialplan but cannot move the already-confirmed target dialog.
    let user = standard_test_users()
        .into_iter()
        .find(|user| user.username == "charlie")
        .unwrap();
    let mut second_device = TestUa::new(TestUaConfig {
        username: user.username,
        password: user.password.unwrap(),
        realm: "127.0.0.1".into(),
        local_port: portpicker::pick_unused_port().unwrap(),
        proxy_addr: format!("127.0.0.1:{}", peers[1].sip_port).parse().unwrap(),
        webrtc: false,
        extra_invite_headers: vec![],
    });
    second_device.start().await.unwrap();
    second_device.register().await.unwrap();
    let replacement_aor = format!("sip:charlie@127.0.0.1:{}", peers[1].sip_port);
    let locations = nodes[1]
        .sip_server()
        .get_inner()
        .locator
        .lookup(&replacement_aor.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(locations.len(), 2, "both devices must remain registered");
    assert_eq!(
        locations[0].home_proxy.as_ref().unwrap().addr.to_string(),
        format!("127.0.0.1:{}", peers[1].sip_port),
        "the new device must be the preferred registration"
    );

    let replaces = format!(
        "{};to-tag={};from-tag={}",
        consultation.call_id, consultation.remote_tag, consultation.local_tag
    );
    let refer_to = format!(
        "sip:charlie@127.0.0.1:{}?Replaces={}",
        peers[1].sip_port, urlencoding::encode(&replaces)
    );
    assert_eq!(transferor.send_refer(&customer_leg, &refer_to).await.unwrap(), 202);
    let referred = timeout(Duration::from_secs(5), async {
        loop {
            transferor.process_dialog_events().await.unwrap();
            for event in customer.process_dialog_events().await.unwrap() {
                if let TestUaEvent::Referred(id, target) = event {
                    assert_eq!(id, original);
                    return target;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("customer must receive the relayed REFER");

    assert_ne!(target_leg.call_id, consultation.call_id);
    let (new_call, _) = timeout(Duration::from_secs(3), async {
        tokio::join!(customer.execute_referred(&referred, Some(sdp.clone())), async {
            let id = next_invite(target).await;
            let on_wire = target.received_replaces(&id).await;
            assert_eq!(
                on_wire.as_deref(),
                Some(format!(
                    "{};to-tag={};from-tag={}",
                    target_leg.call_id, target_leg.local_tag, target_leg.remote_tag
                ).as_str()),
                "the target must receive its own confirmed consultation dialog"
            );
            target.answer_call(&id, Some(sdp.clone())).await.unwrap();
        })
    }).await.expect("the transfer must reach the target without taking the busy-tone path");
    let new_call = new_call.expect("the customer must stay connected to the target");
    customer.send_refer_notify(&original, 200, "refer;id=1").await.unwrap();
    timeout(Duration::from_secs(5), async {
        let mut completed = false;
        let mut released = false;
        while !(completed && released) {
            for event in transferor.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(id, 200) if id == customer_leg => completed = true,
                    TestUaEvent::CallTerminated(id) if id == customer_leg => released = true,
                    _ => {}
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("A must receive a successful commit and exit after the takeover");
    customer.hangup(&new_call).await.unwrap();
}
