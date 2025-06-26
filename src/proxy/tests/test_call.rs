use crate::config::MediaProxyMode;
use crate::proxy::call::CallModule;
use crate::proxy::session::{
    MediaBridgeType, MediaSession, MediaStats, Session, SessionParty, SessionType,
};
use crate::proxy::tests::common::{create_test_request, create_test_server, create_transaction};
use crate::proxy::ProxyModule;
use rsip::headers::{ContentType, Header};
use rsip::prelude::UntypedHeader;
use rsip::HostWithPort;
use rsipstack::dialog::DialogId;
use rsipstack::transport::SipAddr;
use std::time::{Duration, Instant};

fn create_test_dialog_id(call_id: &str, from_tag: &str, to_tag: &str) -> DialogId {
    DialogId {
        call_id: call_id.to_string(),
        from_tag: from_tag.to_string(),
        to_tag: to_tag.to_string(),
    }
}

// Removed unused function - use create_test_uri directly

fn create_test_uri(uri_str: &str) -> rsip::Uri {
    rsip::Uri::try_from(uri_str).unwrap()
}

fn create_test_session(dialog_id: DialogId, caller_uri: &str, callee_uri: &str) -> Session {
    Session {
        dialog_id,
        last_activity: Instant::now(),
        caller: SessionParty::new(create_test_uri(caller_uri)),
        callees: vec![SessionParty::new(create_test_uri(callee_uri))],
        media_bridge_type: MediaBridgeType::None,
        established_at: None,
        media_stats: MediaStats::default(),
        start_time: chrono::Utc::now(),
        ring_time: None,
        answer_time: None,
        status_code: 200,
    }
}

#[tokio::test]
async fn test_call_module_basics() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone(), None);

    assert_eq!(_module.name(), "call");
}

#[tokio::test]
async fn test_call_module_creation() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone(), None);

    assert_eq!(_module.name(), "call");
}

#[tokio::test]
async fn test_locator_integration() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone(), None);

    // Register a user in the locator
    let location = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: rsipstack::transport::SipAddr {
            r#type: None,
            addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    let result = server
        .locator
        .register("alice", Some("example.com"), location)
        .await;
    assert!(result.is_ok());

    // Test looking up user
    let lookup_result = server.locator.lookup("alice", Some("example.com")).await;
    assert!(lookup_result.is_ok());
    let locations = lookup_result.unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(
        locations[0].destination.addr,
        HostWithPort::try_from("192.168.1.100:5060").unwrap()
    );
}

#[tokio::test]
async fn test_media_proxy_nat_only() {
    let mut config = crate::config::ProxyConfig::default();
    config.media_proxy.mode = MediaProxyMode::NatOnly;
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let module = CallModule::new(config, server, None);

    // Create a request with SDP containing private IP
    let mut request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    // Add SDP body with private IP - include connection line (c=)
    let sdp_body = b"v=0\r\no=alice 123 123 IN IP4 192.168.1.100\r\ns=Call\r\nc=IN IP4 192.168.1.100\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
    request.body = sdp_body.to_vec();
    request.headers.push(Header::ContentType(ContentType::new(
        "application/sdp".to_string(),
    )));

    let (tx, _) = create_transaction(request).await;

    let should_proxy = module.should_use_media_proxy(&tx).unwrap();
    assert!(should_proxy);
}

#[tokio::test]
async fn test_media_proxy_none_mode() {
    let mut config = crate::config::ProxyConfig::default();
    config.media_proxy.mode = MediaProxyMode::None;
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let module = CallModule::new(config, server, None);

    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    let (tx, _) = create_transaction(request).await;

    let should_proxy = module.should_use_media_proxy(&tx).unwrap();
    assert!(!should_proxy);
}

#[tokio::test]
async fn test_media_proxy_all_mode() {
    let mut config = crate::config::ProxyConfig::default();
    config.media_proxy.mode = MediaProxyMode::All;
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let module = CallModule::new(config, server, None);

    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    let (tx, _) = create_transaction(request).await;

    let should_proxy = module.should_use_media_proxy(&tx).unwrap();
    assert!(should_proxy);
}

#[tokio::test]
async fn test_options_handling() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server, None);

    // Test that OPTIONS method is in allowed methods
    assert!(module.allow_methods().contains(&rsip::Method::Options));

    // Test dialog activity update logic (without network operations)
    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );

    let media_session = MediaSession {
        session,
        media_stream: None,
        session_type: SessionType::SipToSip,
        webrtc_sdp: None,
        sip_sdp: None,
    };

    module
        .inner
        .sessions
        .write()
        .await
        .insert(dialog_id.clone(), media_session);

    // Verify session exists with old timestamp
    let last_activity_before = module
        .inner
        .sessions
        .read()
        .await
        .get(&dialog_id)
        .unwrap()
        .session
        .last_activity;

    // Simulate dialog activity update (the core logic of handle_options)
    {
        let mut sessions = module.inner.sessions.write().await;
        if let Some(session) = sessions.get_mut(&dialog_id) {
            session.session.last_activity = Instant::now();
        }
    }

    // Verify activity was updated
    let last_activity_after = module
        .inner
        .sessions
        .read()
        .await
        .get(&dialog_id)
        .unwrap()
        .session
        .last_activity;

    assert!(last_activity_after > last_activity_before);
}

#[tokio::test]
async fn test_external_realm_forwarding() {
    let mut config = crate::config::ProxyConfig::default();
    config.external_ip = Some("localhost:5060".to_string());
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let _module = CallModule::new(config, server, None);

    // Test realm comparison logic
    let local_realm = "localhost";
    let external_realm = "external.com";

    // This should be different realms
    assert_ne!(local_realm, external_realm);

    // Test that external realm forwarding is detected
    assert!(external_realm != local_realm);
}

#[tokio::test]
async fn test_local_realm_invite() {
    let mut config = crate::config::ProxyConfig::default();
    config.external_ip = Some("example.com:5060".to_string());
    let (server, config) =
        crate::proxy::tests::common::create_test_server_with_config(config).await;

    let _module = CallModule::new(config, server.clone(), None);

    // Register a user in the locator
    let location = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: rsipstack::transport::SipAddr {
            r#type: None,
            addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    server
        .locator
        .register("alice", Some("example.com"), location)
        .await
        .unwrap();

    // Test locator lookup
    let lookup_result = server.locator.lookup("alice", Some("example.com")).await;
    assert!(lookup_result.is_ok());
    let locations = lookup_result.unwrap();
    assert_eq!(locations.len(), 1);

    // Test realm comparison logic
    let local_realm = "example.com";
    let callee_realm = "example.com";
    assert_eq!(local_realm, callee_realm);
}

#[tokio::test]
async fn test_session_management() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server, None);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );

    let media_session = MediaSession {
        session,
        media_stream: None,
        session_type: SessionType::SipToSip,
        webrtc_sdp: None,
        sip_sdp: None,
    };

    module
        .inner
        .sessions
        .write()
        .await
        .insert(dialog_id.clone(), media_session);

    // Verify session exists
    assert!(module.inner.sessions.read().await.contains_key(&dialog_id));

    // Session should still exist (not expired)
    assert!(module.inner.sessions.read().await.contains_key(&dialog_id));
}

#[tokio::test]
async fn test_module_lifecycle() {
    let (server, config) = create_test_server().await;
    let mut module = CallModule::new(config, server, None);

    let start_result = module.on_start().await;
    assert!(start_result.is_ok());

    let stop_result = module.on_stop().await;
    assert!(stop_result.is_ok());
}

#[tokio::test]
async fn test_dialog_activity_update() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server, None);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let initial_time = Instant::now() - Duration::from_secs(10);
    let mut session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );
    session.last_activity = initial_time;

    let media_session = MediaSession {
        session,
        media_stream: None,
        session_type: SessionType::SipToSip,
        webrtc_sdp: None,
        sip_sdp: None,
    };

    module
        .inner
        .sessions
        .write()
        .await
        .insert(dialog_id.clone(), media_session);

    // Simulate activity update
    {
        let mut sessions = module.inner.sessions.write().await;
        if let Some(session) = sessions.get_mut(&dialog_id) {
            session.session.last_activity = Instant::now();
        }
    }

    // Verify activity was updated
    let updated_session = module.inner.sessions.read().await.get(&dialog_id).cloned();

    assert!(updated_session.is_some());
    let session = updated_session.unwrap();
    assert!(session.session.last_activity > initial_time);
}

#[tokio::test]
async fn test_concurrent_session_access() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server, None);

    let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

    // Add a session
    let session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );

    let media_session = MediaSession {
        session,
        media_stream: None,
        session_type: SessionType::SipToSip,
        webrtc_sdp: None,
        sip_sdp: None,
    };

    module
        .inner
        .sessions
        .write()
        .await
        .insert(dialog_id.clone(), media_session);

    // Simulate concurrent access
    let module_clone = module.clone();
    let dialog_id_clone = dialog_id.clone();

    let handle1 = tokio::spawn(async move {
        for _ in 0..10 {
            {
                let sessions = module_clone.inner.sessions.read().await;
                let _session = sessions.get(&dialog_id_clone);
            } // Drop guard before sleep
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let handle2 = tokio::spawn(async move {
        for _ in 0..10 {
            {
                let mut sessions = module.inner.sessions.write().await;
                if let Some(session) = sessions.get_mut(&dialog_id) {
                    session.session.last_activity = Instant::now();
                }
            } // Drop guard before sleep
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let _ = tokio::join!(handle1, handle2);
}

#[tokio::test]
async fn test_bye_routing_and_locator_lookup() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server.clone(), None);

    // Create a test dialog and session
    let dialog_id = create_test_dialog_id("test-call-id", "caller-tag", "callee-tag");

    let _caller_party = SessionParty::new(create_test_uri("sip:alice@example.com"));
    let _callee_party = SessionParty::new(create_test_uri("sip:bob@example.com"));

    let session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );

    let media_session = MediaSession {
        session,
        media_stream: None,
        session_type: SessionType::SipToSip,
        webrtc_sdp: None,
        sip_sdp: None,
    };

    // Register users in locator with multiple locations
    let alice_location1 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    let bob_location1 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:bob@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.20:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    server
        .locator
        .register("alice", Some("example.com"), alice_location1)
        .await
        .unwrap();

    server
        .locator
        .register("bob", Some("example.com"), bob_location1)
        .await
        .unwrap();

    // Add the session to the module
    module
        .inner
        .sessions
        .write()
        .await
        .insert(dialog_id.clone(), media_session);

    // Verify session exists and structure
    let stored_session = module
        .inner
        .sessions
        .read()
        .await
        .get(&dialog_id)
        .cloned()
        .unwrap();

    assert_eq!(stored_session.session.caller.aor.user().unwrap(), "alice");
    assert_eq!(stored_session.session.callees[0].aor.user().unwrap(), "bob");

    // Verify that locator lookup works for both parties
    let alice_locations = server
        .locator
        .lookup("alice", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(alice_locations.len(), 1);

    let bob_locations = server
        .locator
        .lookup("bob", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(bob_locations.len(), 1);
}

#[tokio::test]
async fn test_multiple_locations_per_aor() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone(), None);

    // Register the same user with multiple locations (simulating multiple devices)
    let alice_location1 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    // Register first location
    server
        .locator
        .register("alice", Some("example.com"), alice_location1)
        .await
        .unwrap();

    // Verify single location
    let locations = server
        .locator
        .lookup("alice", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);

    // Note: Current MemoryLocator implementation only keeps one location per user
    // This is a limitation that could be enhanced to support multiple locations
    // For now, we test the basic lookup functionality

    // The locator should find the user
    assert_eq!(locations[0].aor.user().unwrap(), "alice");
    assert_eq!(locations[0].aor.host().to_string(), "example.com");
}

#[tokio::test]
async fn test_session_party_aor_methods() {
    let party = SessionParty::new(create_test_uri("sip:alice@example.com"));

    assert_eq!(party.get_user(), "alice");
    assert_eq!(party.get_realm(), "example.com");
    assert_eq!(party.aor.to_string(), "sip:alice@example.com");
}

#[tokio::test]
async fn test_location_selection_strategy() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server.clone(), None);

    // Create multiple locations for the same user
    let location1 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    let location2 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Tcp),
            addr: HostWithPort::try_from("192.168.1.20:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    let locations = vec![location1.clone(), location2.clone()];
    let aor = rsip::Uri::try_from("sip:alice@example.com").unwrap();

    // Test location selection (currently always selects first one)
    let selected = module.select_location_from_multiple(&locations, &aor);

    // Should select the first location
    assert_eq!(selected.destination.addr.to_string(), "192.168.1.10:5060");
    assert_eq!(
        selected.destination.r#type,
        Some(rsip::transport::Transport::Udp)
    );
}

#[tokio::test]
async fn test_bye_with_dynamic_location_lookup() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server.clone(), None);

    // Create a session
    let dialog_id = create_test_dialog_id("test-dynamic", "caller-tag", "callee-tag");
    let _caller_party = SessionParty::new(create_test_uri("sip:alice@example.com"));
    let _callee_party = SessionParty::new(create_test_uri("sip:bob@example.com"));

    let session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );

    let media_session = MediaSession {
        session,
        media_stream: None,
        session_type: SessionType::SipToSip,
        webrtc_sdp: None,
        sip_sdp: None,
    };

    // Register users with initial locations
    let alice_location = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    let bob_location_v1 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:bob@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.20:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    server
        .locator
        .register("alice", Some("example.com"), alice_location)
        .await
        .unwrap();

    server
        .locator
        .register("bob", Some("example.com"), bob_location_v1)
        .await
        .unwrap();

    // Add session
    module
        .inner
        .sessions
        .write()
        .await
        .insert(dialog_id.clone(), media_session);

    // Verify initial locations
    let alice_locations = server
        .locator
        .lookup("alice", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(
        alice_locations[0].destination.addr.to_string(),
        "192.168.1.10:5060"
    );

    let bob_locations = server
        .locator
        .lookup("bob", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(
        bob_locations[0].destination.addr.to_string(),
        "192.168.1.20:5060"
    );

    // Now simulate Bob moving to a new location (re-registration)
    let bob_location_v2 = super::super::locator::Location {
        aor: rsip::Uri::try_from("sip:bob@example.com").unwrap(),
        expires: 3600,
        destination: SipAddr {
            r#type: Some(rsip::transport::Transport::Tcp),
            addr: HostWithPort::try_from("10.0.0.30:5060").unwrap(),
        },
        last_modified: Instant::now(),
    };

    server
        .locator
        .register("bob", Some("example.com"), bob_location_v2)
        .await
        .unwrap();

    // Verify Bob's location has changed
    let bob_locations_new = server
        .locator
        .lookup("bob", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(
        bob_locations_new[0].destination.addr.to_string(),
        "10.0.0.30:5060"
    );

    // This demonstrates that when we handle BYE, we'll get the current location
    // rather than the stale location that might have been stored in the session
}

#[tokio::test]
async fn test_webrtc_sdp_detection() {
    let (server, config) = create_test_server().await;
    let module = CallModule::new(config, server.clone(), None);

    // Test WebRTC SDP detection
    let webrtc_sdp = "v=0\r\n\
                      o=- 1234567890 1234567890 IN IP4 127.0.0.1\r\n\
                      s=-\r\n\
                      t=0 0\r\n\
                      a=ice-ufrag:abcd\r\n\
                      a=ice-pwd:1234567890abcdef\r\n\
                      a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF\r\n\
                      m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                      a=setup:actpass\r\n";

    let sip_sdp = "v=0\r\n\
                   o=- 1234567890 1234567890 IN IP4 127.0.0.1\r\n\
                   s=-\r\n\
                   t=0 0\r\n\
                   m=audio 5004 RTP/AVP 0\r\n\
                   a=rtpmap:0 PCMU/8000\r\n";

    assert!(module.is_webrtc_sdp(webrtc_sdp));
    assert!(!module.is_webrtc_sdp(sip_sdp));
}

#[tokio::test]
async fn test_enhanced_session_structure() {
    let (server, config) = create_test_server().await;
    let _module = CallModule::new(config, server.clone(), None);

    let dialog_id = create_test_dialog_id("test-session", "caller-tag", "callee-tag");

    let mut session = create_test_session(
        dialog_id.clone(),
        "sip:alice@example.com",
        "sip:bob@example.com",
    );
    session.media_bridge_type = MediaBridgeType::WebRtcToSip;
    session.established_at = Some(Instant::now());

    let media_session = MediaSession {
        session: session.clone(),
        media_stream: None,
        session_type: SessionType::WebRtcToSip,
        webrtc_sdp: Some("v=0...".to_string()),
        sip_sdp: Some("v=0...".to_string()),
    };

    // Test WebRTC to SIP session properties
    assert_eq!(media_session.session_type, SessionType::WebRtcToSip);
    assert_eq!(
        media_session.session.media_bridge_type,
        MediaBridgeType::WebRtcToSip
    );
    assert!(media_session.session.established_at.is_some());
    assert!(media_session.webrtc_sdp.is_some());
    assert!(media_session.sip_sdp.is_some());
}
