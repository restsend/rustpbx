use super::common::*;
use crate::call::TransactionCookie;
use crate::proxy::ProxyAction;
use crate::proxy::presence::{PresenceModule, PresenceStatus};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_presence_workflow() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();

    // 1. Initial check
    assert_eq!(manager.get_state("bob").status, PresenceStatus::Offline);

    // 2. Alice subscribes to Bob
    let mut alice_subscribe = create_test_request(
        rsipstack::sip::Method::Subscribe,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let to_bob = rsipstack::sip::typed::To {
        display_name: None,
        uri: "sip:bob@rustpbx.com".try_into().unwrap(),
        params: vec![],
    };
    alice_subscribe
        .headers
        .retain(|h| !matches!(h, rsipstack::sip::Header::To(_)));
    alice_subscribe
        .headers
        .push(rsipstack::sip::Header::To(to_bob.into()));

    let module = PresenceModule::create(server.clone(), config.clone()).unwrap();

    let (mut tx_sub, _) = create_transaction(alice_subscribe).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_sub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, ProxyAction::Abort));

    // Verify Alice is a subscriber
    let subs = manager.get_subscribers("bob");
    assert_eq!(subs.len(), 1);
    assert!(subs[0].aor.to_string().contains("alice@rustpbx.com"));

    // 3. Bob publishes Busy
    let mut bob_publish_busy = create_test_request(
        rsipstack::sip::Method::Publish,
        "bob",
        None,
        "rustpbx.com",
        None,
    );
    bob_publish_busy.body = "status: busy".as_bytes().to_vec();

    let (mut tx_pub, _) = create_transaction(bob_publish_busy).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_pub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, ProxyAction::Abort));

    // 4. Verify Bob status in manager
    assert_eq!(manager.get_state("bob").status, PresenceStatus::Busy);
}

#[tokio::test]
async fn test_presence_locator_sync() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let mut module = PresenceModule::create(server.clone(), config.clone()).unwrap();

    // Start module to listen for events
    module.on_start().await.unwrap();

    // Initially offline
    assert_eq!(manager.get_state("alice").status, PresenceStatus::Offline);

    // Mock a Registration event
    let location = crate::call::Location {
        aor: "sip:alice@rustpbx.com".try_into().unwrap(),
        ..Default::default()
    };

    if let Some(tx) = &server.locator_events {
        tx.send(crate::proxy::locator::LocatorEvent::Registered(location))
            .unwrap();
    }

    // Give some time for event processing
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Alice should now be Idle
    assert_eq!(manager.get_state("alice").status, PresenceStatus::Idle);
}

#[tokio::test]
async fn test_presence_notify_body_generation() {
    use crate::proxy::presence::build_pidf_body;
    let (server, _config) = create_test_server().await;
    let manager = server.presence_manager.clone();

    // Set up a known state via update_state
    manager
        .update_state(
            "bob",
            crate::proxy::presence::PresenceState {
                status: crate::proxy::presence::PresenceStatus::Busy,
                note: None,
                activity: None,
                last_updated: chrono::Utc::now().timestamp(),
            },
            &crate::proxy::cluster_event::EventSource::Local,
        )
        .await;

    // Verify the state is stored correctly
    let state = manager.get_state("bob");
    assert_eq!(state.status, crate::proxy::presence::PresenceStatus::Busy);

    // Verify build_pidf_body produces valid output for this state
    let body = build_pidf_body("bob", "rustpbx.com", &state);
    assert!(body.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
    assert!(body.contains("entity=\"sip:bob@rustpbx.com\""));
    assert!(body.contains("<basic>open</basic>"));
    assert!(body.contains("<note>busy</note>"));

    // Change to idle and verify
    manager
        .update_state(
            "bob",
            crate::proxy::presence::PresenceState {
                status: crate::proxy::presence::PresenceStatus::Idle,
                note: None,
                activity: None,
                last_updated: chrono::Utc::now().timestamp(),
            },
            &crate::proxy::cluster_event::EventSource::Local,
        )
        .await;

    let state = manager.get_state("bob");
    assert_eq!(state.status, crate::proxy::presence::PresenceStatus::Idle);

    let body = build_pidf_body("bob", "rustpbx.com", &state);
    assert!(body.contains("<basic>open</basic>"));
    assert!(body.contains("<note>idle</note>"));

    // Change to wrapup and verify
    manager
        .update_state(
            "bob",
            crate::proxy::presence::PresenceState {
                status: crate::proxy::presence::PresenceStatus::Wrapup,
                note: None,
                activity: None,
                last_updated: chrono::Utc::now().timestamp(),
            },
            &crate::proxy::cluster_event::EventSource::Local,
        )
        .await;

    let state = manager.get_state("bob");
    assert_eq!(state.status, crate::proxy::presence::PresenceStatus::Wrapup);

    let body = build_pidf_body("bob", "rustpbx.com", &state);
    assert!(body.contains("<note>wrapup</note>"));
    assert!(body.contains("<basic>open</basic>"));

    // Change to offline and verify
    manager
        .update_state(
            "bob",
            crate::proxy::presence::PresenceState {
                status: crate::proxy::presence::PresenceStatus::Offline,
                note: None,
                activity: None,
                last_updated: chrono::Utc::now().timestamp(),
            },
            &crate::proxy::cluster_event::EventSource::Local,
        )
        .await;

    let state = manager.get_state("bob");
    assert_eq!(
        state.status,
        crate::proxy::presence::PresenceStatus::Offline
    );

    let body = build_pidf_body("bob", "rustpbx.com", &state);
    assert!(body.contains("<basic>closed</basic>"));
    assert!(body.contains("<note>offline</note>"));
}
