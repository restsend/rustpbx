use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::TestUaEvent;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Yealink network conference: REFER on each old dialog to the room URI.
/// One old dialog was incoming to B and the other outgoing.
#[tokio::test]
async fn test_network_conference_factory_and_existing_calls() {
    let _ = tracing_subscriber::fmt::try_init();
    let port = portpicker::pick_unused_port().unwrap();
    let mut config = crate::common::test_helpers::test_proxy_config(port);
    config.media_proxy = MediaProxyMode::All;
    let factory = format!("sip:conference@127.0.0.1:{port}");
    config.conference_factory_uri = Some(factory.clone());
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject { users, ..Default::default() }).await.unwrap());
    let factory = format!("sip:conference@{}", server.proxy_addr);
    let mut effective = (*server.server_ref.proxy_config.load_full()).clone();
    effective.conference_factory_uri = Some(factory.clone());
    server.server_ref.proxy_config.store(Arc::new(effective));
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut original = Vec::new();
    for (caller, callee, name) in [(alice.clone(), bob.clone(), "bob"), (bob.clone(), charlie.clone(), "charlie")] {
        let dial = rustpbx::utils::spawn({ let caller = caller.clone(); let sdp = sdp.clone(); async move { caller.make_call(name, Some(sdp)).await.unwrap() } });
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let sent = dial.await.unwrap();
        caller.set_answer_sdp(&sent, &sdp).await;
        callee.set_answer_sdp(&received, &sdp).await;
        original.push((sent, received));
    }
    bob.send_reinvite(&original[0].1, Some(sdp.replace("sendrecv", "sendonly"))).await.unwrap();
    let focus_dialog = bob.make_call("conference", Some(sdp.clone())).await.unwrap();
    let room = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(room) = server.server_ref.conference_server.list_conferences_detail().await.into_iter().find(|room| room.focus_uri.is_some() && room.participant_count() == 1) { return room; }
            bob.process_dialog_events().await.unwrap();
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_ne!(room.focus_uri.as_deref(), Some(factory.as_str()));
    // Discover the actual focus through OPTIONS, including the normal
    // out-of-dialog filtering in SipServer.
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    let room_uri = room.focus_uri.as_ref().unwrap();
    let options = format!("OPTIONS {room_uri} SIP/2.0\r\nVia: SIP/2.0/UDP {local};branch=z9hG4bKconference-options;rport\r\nFrom: <sip:bob@127.0.0.1>;tag=discovery\r\nTo: <{room_uri}>\r\nCall-ID: conference-discovery\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n");
    socket.send_to(options.as_bytes(), server.proxy_addr).await.unwrap();
    let mut response = [0u8; 4096];
    let count = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut response)).await.unwrap().unwrap();
    let response = String::from_utf8_lossy(&response[..count]);
    assert!(response.starts_with("SIP/2.0 200"), "{response}");
    assert!(response.contains(";isfocus"), "{response}");
    assert_eq!(response.lines().filter(|line| line.to_ascii_lowercase().starts_with("contact:")).count(), 1);
    let dial_in = alice.make_call(&room.id.0, Some(sdp.clone())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if server.server_ref.conference_server.get_conference(&room.id).await.unwrap().participant_count() == 2 { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    alice.hangup(&dial_in).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if server.server_ref.conference_server.get_conference(&room.id).await.unwrap().participant_count() == 1 { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let registry = &server.server_ref.active_call_registry;
    let a_owner = registry.get_handle_by_dialog(&original[0].0.call_id).unwrap();
    let c_owner = registry.get_handle_by_dialog(&original[1].0.call_id).unwrap();
    alice.process_dialog_events().await.unwrap();
    charlie.process_dialog_events().await.unwrap();
    for old in [original[0].1.clone(), original[1].0.clone()] {
        assert_eq!(bob.send_refer(&old, room_uri).await.unwrap(), 202);
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                for (ua, expected) in [(&alice, &original[0].0), (&charlie, &original[1].1)] {
                    let events = ua.process_dialog_events().await.unwrap();
                    assert!(!events.iter().any(|event| match event {
                        TestUaEvent::IncomingCall(id, _) => id.call_id != expected.call_id && id.call_id != dial_in.call_id,
                        TestUaEvent::CallTerminated(id) => id == expected,
                        _ => false,
                    }), "existing A/C dialog must survive migration: {events:?}");
                }
                for event in bob.process_dialog_events().await.unwrap() {
                    assert!(!matches!(&event, TestUaEvent::CallTerminated(id) if id == &old),
                        "PBX must wait for the transferor's BYE");
                    if let TestUaEvent::ReferNotify(_, body, state) = event {
                        if state.starts_with("terminated") { assert!(body.contains("200 OK"), "{body}"); return; }
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        bob.hangup(&old).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let old_b_dialogs = [original[0].1.clone(), original[1].0.clone()];
            if old_b_dialogs.iter().all(|id| {
                let peer_id = rsipstack::dialog::DialogId {
                    call_id: id.call_id.clone(), local_tag: id.remote_tag.clone(), remote_tag: id.local_tag.clone(),
                };
                server.server_ref.dialog_layer.get_dialog(&peer_id).is_none()
            }) { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_eq!(server.server_ref.active_call_registry.count(), 3, "No self-dialed conference sessions");
    assert_eq!(server.server_ref.conference_server.get_conference(&room.id).await.unwrap().participant_count(), 3);
    for (owner, leg) in [(a_owner, "caller"), (c_owner, "callee")] {
        let participant = rustpbx::call::domain::LegId::from(format!("{}-{leg}", owner.session_id()));
        assert_eq!(server.server_ref.conference_server.get_conference_id_for_leg(&participant).await, Some(room.id.clone()));
    }
    alice.hangup(&original[0].0).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.server_ref.conference_server.get_conference(&room.id).await.unwrap().participant_count() == 2 { break; }
            alice.process_dialog_events().await.unwrap(); bob.process_dialog_events().await.unwrap(); charlie.process_dialog_events().await.unwrap();
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    bob.hangup(&focus_dialog).await.unwrap();
    let mut charlie_ended = false;
    // Room teardown must issue BYE immediately, not after the 3s drain timeout.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            for event in charlie.process_dialog_events().await.unwrap() {
                if matches!(event, TestUaEvent::CallTerminated(id) if id == original[1].1) {
                    charlie_ended = true;
                }
            }
            if charlie_ended && server.server_ref.conference_server.get_conference(&room.id).await.is_none()
                && server.server_ref.active_call_registry.count() == 0 { break; }
            bob.process_dialog_events().await.unwrap();
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("Creator leaving must destroy the room and hang up C");
    let expired = alice.make_call(&room.id.0, Some(sdp)).await.unwrap_err();
    assert!(expired.to_string().contains("404"), "expired room must not route as a normal user: {expired}");
    server.stop();
}

/// A -> B and B -> C remain separate sessions; REFER must reuse A/C dialogs.
#[tokio::test]
async fn test_attended_refer_existing_cross_session_dialogs() {
    for caller_leaves in [true, false] {
    let _ = tracing_subscriber::fmt::try_init();
    let port = portpicker::pick_unused_port().unwrap();
    let mut config = crate::common::test_helpers::test_proxy_config(port);
    config.media_proxy = MediaProxyMode::All;
    let factory = format!("sip:conference@127.0.0.1:{port}");
    config.conference_factory_uri = Some(factory.clone());
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject { users, ..Default::default() }).await.unwrap());
    let factory = format!("sip:conference@{}", server.proxy_addr);
    let mut effective = (*server.server_ref.proxy_config.load_full()).clone();
    effective.conference_factory_uri = Some(factory.clone());
    server.server_ref.proxy_config.store(Arc::new(effective));
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut original = Vec::new();
    for (caller, callee, name) in [(alice.clone(), bob.clone(), "bob"), (bob.clone(), charlie.clone(), "charlie")] {
        let dial = rustpbx::utils::spawn({ let caller = caller.clone(); let sdp = sdp.clone(); async move { caller.make_call(name, Some(sdp)).await.unwrap() } });
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let sent = dial.await.unwrap();
        caller.set_answer_sdp(&sent, &sdp).await;
        callee.set_answer_sdp(&received, &sdp).await;
        original.push((sent, received));
    }
    bob.send_reinvite(&original[0].1, Some(sdp.replace("sendrecv", "sendonly"))).await.unwrap();

    let registry = &server.server_ref.active_call_registry;
    let a_owner = registry.get_handle_by_dialog(&original[0].0.call_id).unwrap();
    let c_owner = registry.get_handle_by_dialog(&original[1].0.call_id).unwrap();
    assert_ne!(a_owner.session_id(), c_owner.session_id());
    let consult = &original[1].0;
    for (tag, expected) in [("wrong-tag".to_string(), 481), (consult.remote_tag.clone(), 200)] {
        let replaces = format!("{};to-tag={};from-tag={}", consult.call_id, tag, consult.local_tag);
        let target = format!("sip:charlie@{}?Replaces={}", server.proxy_addr, urlencoding::encode(&replaces));
        assert_eq!(bob.send_refer(&original[0].1, &target).await.unwrap(), 202);
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                for (ua, existing) in [(&alice, &original[0].0), (&charlie, &original[1].1)] {
                    let events = ua.process_dialog_events().await.unwrap();
                    assert!(!events.iter().any(|event| matches!(event, TestUaEvent::IncomingCall(id, _) if id.call_id != existing.call_id)),
                        "attended REFER must not originate another call: {events:?}");
                    assert!(!events.iter().any(|event| matches!(event, TestUaEvent::CallTerminated(..))),
                        "existing participants must survive: {events:?}");
                }
                for event in bob.process_dialog_events().await.unwrap() {
                    assert!(!matches!(event, TestUaEvent::CallTerminated(_)), "B owns its BYEs");
                    if let TestUaEvent::ReferNotify(_, body, state) = event {
                        if state.starts_with("terminated") {
                            assert!(body.contains(&format!("SIP/2.0 {expected}")), "{body}");
                            return;
                        }
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
    }
    let room = server.server_ref.conference_server.list_conferences_detail().await.into_iter()
        .find(|room| room.host_leg_id == Some(rustpbx::call::domain::LegId::from(format!("{}-caller", a_owner.session_id()))))
        .expect("surviving caller owns the transfer room");
    assert_eq!(room.participant_count(), 2);
    assert!(room.focus_uri.is_none());
    assert_eq!(registry.count(), 2, "no replacement INVITE/session");
    for (owner, leg) in [(&a_owner, "caller"), (&c_owner, "callee")] {
        let participant = rustpbx::call::domain::LegId::from(format!("{}-{leg}", owner.session_id()));
        assert_eq!(server.server_ref.conference_server.get_conference_id_for_leg(&participant).await, Some(room.id.clone()));
    }
    bob.hangup(&original[0].1).await.unwrap();
    bob.hangup(&original[1].0).await.unwrap();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(registry.count(), 2);
    assert_eq!(server.server_ref.conference_server.get_conference(&room.id).await.unwrap().participant_count(), 2);
    if caller_leaves { alice.hangup(&original[0].0).await.unwrap(); }
    else { charlie.hangup(&original[1].1).await.unwrap(); }
    let mut ended = false;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (survivor, expected) = if caller_leaves { (&charlie, &original[1].1) } else { (&alice, &original[0].0) };
            for event in survivor.process_dialog_events().await.unwrap() {
                if matches!(event, TestUaEvent::CallTerminated(id) if &id == expected) { ended = true; }
            }
            if ended && registry.count() == 0 && server.server_ref.conference_server.get_conference(&room.id).await.is_none() { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("either participant leaving must end the transferred call");
    server.stop();
    }
}

#[tokio::test]
async fn test_blind_refer_from_replacement_leg() {
    let _ = tracing_subscriber::fmt::try_init();
    let port = portpicker::pick_unused_port().unwrap();
    let mut config = crate::common::test_helpers::test_proxy_config(port);
    config.media_proxy = MediaProxyMode::All;
    let factory = format!("sip:conference@127.0.0.1:{port}");
    config.conference_factory_uri = Some(factory.clone());
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject { users, ..Default::default() }).await.unwrap());
    let factory = format!("sip:conference@{}", server.proxy_addr);
    let mut effective = (*server.server_ref.proxy_config.load_full()).clone();
    effective.conference_factory_uri = Some(factory.clone());
    server.server_ref.proxy_config.store(Arc::new(effective));
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut original = Vec::new();
    for (caller, callee, name) in [(alice.clone(), bob.clone(), "bob")] {
        let dial = rustpbx::utils::spawn({ let caller = caller.clone(); let sdp = sdp.clone(); async move { caller.make_call(name, Some(sdp)).await.unwrap() } });
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let sent = dial.await.unwrap();
        caller.set_answer_sdp(&sent, &sdp).await;
        callee.set_answer_sdp(&received, &sdp).await;
        original.push((sent, received));
    }

    let mut current = original[0].1.clone();
    for (referrer, target, name) in [(&bob, &charlie, "charlie"), (&charlie, &bob, "bob")] {
        target.process_dialog_events().await.unwrap();
        let uri = format!("sip:{name}@{}", server.proxy_addr);
        assert_eq!(referrer.send_refer(&current, &uri).await.unwrap(), 202);
        let mut answered = None;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                for event in target.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        if answered.is_none() {
                            target.answer_call(&id, Some(sdp.clone())).await.unwrap();
                            target.set_answer_sdp(&id, &sdp).await;
                            answered = Some(id);
                        }
                    }
                }
                for event in referrer.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::ReferNotify(_, body, state) = event {
                        if state.starts_with("terminated") {
                            assert!(body.contains("200 OK"), "{body}");
                            return;
                        }
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("replacement agent must be able to REFER again");
        referrer.hangup(&current).await.unwrap();
        current = answered.unwrap();
        sleep(Duration::from_millis(50)).await;
        assert_eq!(server.server_ref.active_call_registry.count(), 1);
    }
    bob.hangup(&current).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            alice.process_dialog_events().await.unwrap();
            if server.server_ref.active_call_registry.count() == 0 { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    server.stop();
}

/// A is already attached when C rejects joining; NOTIFY must precede teardown,
/// and C's owning session must be ended even though it never joined that room.
#[tokio::test]
async fn test_attended_refer_attachment_failure_ends_both_sessions() {
    let _ = tracing_subscriber::fmt::try_init();
    let port = portpicker::pick_unused_port().unwrap();
    let mut config = crate::common::test_helpers::test_proxy_config(port);
    config.media_proxy = MediaProxyMode::All;
    let factory = format!("sip:conference@127.0.0.1:{port}");
    config.conference_factory_uri = Some(factory.clone());
    let mut users = crate::common::test_helpers::standard_test_users();
    for user in &mut users { user.is_support_webrtc = false; }
    let server = Arc::new(E2eTestServer::start_with_inject(config, E2eTestServerInject { users, ..Default::default() }).await.unwrap());
    let factory = format!("sip:conference@{}", server.proxy_addr);
    let mut effective = (*server.server_ref.proxy_config.load_full()).clone();
    effective.conference_factory_uri = Some(factory.clone());
    server.server_ref.proxy_config.store(Arc::new(effective));
    let alice = server.create_ua("alice").await.unwrap();
    let bob = server.create_ua("bob").await.unwrap();
    let charlie = server.create_ua("charlie").await.unwrap();
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 23456 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut original = Vec::new();
    for (caller, callee, name) in [(alice.clone(), bob.clone(), "bob"), (bob.clone(), charlie.clone(), "charlie")] {
        let dial = rustpbx::utils::spawn({ let caller = caller.clone(); let sdp = sdp.clone(); async move { caller.make_call(name, Some(sdp)).await.unwrap() } });
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for event in callee.process_dialog_events().await.unwrap() {
                    if let TestUaEvent::IncomingCall(id, _) = event {
                        callee.answer_call(&id, Some(sdp.clone())).await.unwrap();
                        return id;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        let sent = dial.await.unwrap();
        caller.set_answer_sdp(&sent, &sdp).await;
        callee.set_answer_sdp(&received, &sdp).await;
        original.push((sent, received));
    }
    bob.send_reinvite(&original[0].1, Some(sdp.replace("sendrecv", "sendonly"))).await.unwrap();

    let registry = &server.server_ref.active_call_registry;
    let c_owner = registry.get_handle_by_dialog(&original[1].0.call_id).unwrap();
    let manager = &server.server_ref.conference_server;
    let occupied = rustpbx::call::runtime::ConferenceId::from("occupied-consultation");
    manager.create_conference(occupied.clone(), None).await.unwrap();
    manager.set_focus(&occupied, format!("sip:occupied@{}", server.proxy_addr)).unwrap();
    c_owner.send_command(rustpbx::call::domain::CallCommand::JoinMixerLeg {
        mixer_id: occupied.0.clone(), leg_id: rustpbx::call::domain::LegId::from("callee"),
    }).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if manager.get_conference(&occupied).await.unwrap().participant_count() == 1 { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let consult = &original[1].0;
    let replaces = format!("{};to-tag={};from-tag={}", consult.call_id, consult.remote_tag, consult.local_tag);
    let target = format!("sip:charlie@{}?Replaces={}", server.proxy_addr, urlencoding::encode(&replaces));
    assert_eq!(bob.send_refer(&original[0].1, &target).await.unwrap(), 202);
    let mut failure_notified = false;
    let mut alice_ended = false;
    let mut charlie_ended = false;
    let mut transferor_ended = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for event in bob.process_dialog_events().await.unwrap() {
                match event {
                    TestUaEvent::ReferNotify(_, body, state) if state.starts_with("terminated") => {
                        assert!(body.contains("SIP/2.0 500"), "{body}");
                        failure_notified = true;
                    }
                    TestUaEvent::CallTerminated(id) if id == original[0].1 => {
                        assert!(failure_notified, "REFER failure must precede B's BYE");
                        transferor_ended = true;
                    }
                    _ => {}
                }
            }
            for event in alice.process_dialog_events().await.unwrap() {
                if matches!(event, TestUaEvent::CallTerminated(id) if id == original[0].0) { alice_ended = true; }
            }
            for event in charlie.process_dialog_events().await.unwrap() {
                if matches!(event, TestUaEvent::CallTerminated(id) if id == original[1].1) { charlie_ended = true; }
            }
            if failure_notified && transferor_ended && alice_ended && charlie_ended && registry.count() == 0
                && server.server_ref.conference_server.list_conferences_detail().await.is_empty() { break; }
            sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("failed attachment must notify then clean both calls and all rooms");
    server.stop();
}
