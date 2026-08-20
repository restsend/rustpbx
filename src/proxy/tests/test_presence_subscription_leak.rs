//! Strict regression tests for presence/MWI subscription leaks.
//!
//! Covers the failure mode seen in production logs where repeated
//! WebSocket reconnects left dead SUBSCRIBE dialogs in `PresenceManager`,
//! so each state change emitted N "Sending NOTIFY" lines that grew forever.
//!
//! Layers under test:
//! 1. `PresenceManager` bookkeeping (replace / prune / expire / WebRTC identity)
//! 2. `PresenceModule` SUBSCRIBE + NOTIFY-fail prune + locator Offline
//! 3. `ClusterEventHub` remote Offline prune + dialog_layer remove

use super::common::{create_test_request, create_test_server, create_transaction};
use crate::call::{Location, TransactionCookie};
use crate::proxy::ProxyAction;
use crate::proxy::cluster_event::{ClusterEventHub, EventSource};
use crate::proxy::locator::LocatorEvent;
use crate::proxy::presence::{
    MwiSubscriber, PresenceManager, PresenceModule, PresenceState, PresenceStatus, Subscriber,
};
use rsipstack::dialog::DialogId;
use rsipstack::sip::{SipMessage, StatusCode, Uri};
use rsipstack::transaction::key::TransactionRole;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn dialog_id(call_id: &str) -> DialogId {
    DialogId {
        call_id: call_id.into(),
        local_tag: format!("l-{call_id}"),
        remote_tag: format!("r-{call_id}"),
    }
}

fn watcher(user: &str, host: &str) -> Uri {
    Uri::try_from(format!("sip:{user}@{host}")).unwrap()
}

fn presence_sub(aor: Uri, call_id: &str, ttl_secs: u64) -> Subscriber {
    Subscriber {
        aor,
        dialog_id: dialog_id(call_id),
        expires: Instant::now() + Duration::from_secs(ttl_secs),
    }
}

fn mwi_sub(aor: Uri, call_id: &str, ttl_secs: u64) -> MwiSubscriber {
    MwiSubscriber {
        aor: aor.clone(),
        dialog_id: dialog_id(call_id),
        account_uri: format!("sip:mailbox@{}", aor.host()),
        expires: Instant::now() + Duration::from_secs(ttl_secs),
    }
}

fn webrtc_location(extension: &str, contact_user: &str) -> Location {
    Location {
        aor: Uri::try_from(format!(
            "sip:{contact_user}@gc1g9pmgn89n.invalid;transport=ws"
        ))
        .unwrap(),
        expires: 50,
        supports_webrtc: true,
        registered_aor: Some(watcher(extension, "192.168.3.227")),
        ..Default::default()
    }
}

fn subscribe_to(
    from_user: &str,
    to_user: &str,
    realm: &str,
    expires: Option<u32>,
    event: &str,
) -> rsipstack::sip::Request {
    let mut req = create_test_request(
        rsipstack::sip::Method::Subscribe,
        from_user,
        None,
        realm,
        expires,
    );
    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: Uri::try_from(format!("sip:{to_user}@{realm}")).unwrap(),
        params: vec![],
    };
    req.headers
        .retain(|h| !matches!(h, rsipstack::sip::Header::To(_)));
    req.headers.push(rsipstack::sip::Header::To(to.into()));
    req.headers.push(rsipstack::sip::Header::Event(
        rsipstack::sip::headers::Event::new(event),
    ));
    req
}

async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    pred()
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. PresenceManager bookkeeping
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn manager_same_watcher_replaces_and_returns_old_dialog() {
    let manager = PresenceManager::new(None);
    let aor = watcher("1001", "pbx.local");

    let replaced_first = manager.add_subscriber("1001", presence_sub(aor.clone(), "old", 3600));
    assert!(replaced_first.is_empty());
    assert_eq!(manager.subscriber_bindings_len(), 1);

    let replaced = manager.add_subscriber("1001", presence_sub(aor, "new", 3600));
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].call_id, "old");
    assert_eq!(manager.subscriber_bindings_len(), 1);
    assert_eq!(manager.get_subscribers("1001")[0].dialog_id.call_id, "new");
}

#[tokio::test]
async fn manager_watcher_match_is_case_insensitive() {
    let manager = PresenceManager::new(None);
    manager.add_subscriber("bob", presence_sub(watcher("Alice", "PBX.LOCAL"), "c1", 60));
    manager.add_subscriber("bob", presence_sub(watcher("alice", "pbx.local"), "c2", 60));
    // Same watcher key → still one binding
    assert_eq!(manager.subscriber_bindings_len(), 1);
    assert_eq!(manager.get_subscribers("bob")[0].dialog_id.call_id, "c2");
}

#[tokio::test]
async fn manager_distinct_watchers_coexist() {
    let manager = PresenceManager::new(None);
    manager.add_subscriber("bob", presence_sub(watcher("alice", "pbx.local"), "a", 60));
    manager.add_subscriber("bob", presence_sub(watcher("carol", "pbx.local"), "c", 60));
    assert_eq!(manager.subscriber_bindings_len(), 2);
    assert_eq!(manager.get_subscribers("bob").len(), 2);
}

#[tokio::test]
async fn manager_remove_by_dialog_drops_empty_bucket() {
    let manager = PresenceManager::new(None);
    let id = dialog_id("only");
    manager.add_subscriber(
        "bob",
        Subscriber {
            aor: watcher("alice", "pbx.local"),
            dialog_id: id.clone(),
            expires: Instant::now() + Duration::from_secs(60),
        },
    );
    assert!(manager.remove_subscriber_by_dialog(&id));
    assert_eq!(manager.subscriber_bindings_len(), 0);
    assert_eq!(
        manager.subscribers_len(),
        0,
        "empty identity bucket must go"
    );
    assert!(!manager.remove_subscriber_by_dialog(&id));
}

#[tokio::test]
async fn manager_cleanup_expired_keeps_live_and_drops_empty_keys() {
    let manager = PresenceManager::new(None);
    manager.add_subscriber(
        "live",
        presence_sub(watcher("alice", "pbx.local"), "live", 3600),
    );
    manager.add_subscriber(
        "dead",
        Subscriber {
            aor: watcher("alice", "pbx.local"),
            dialog_id: dialog_id("dead"),
            expires: Instant::now() - Duration::from_secs(1),
        },
    );
    manager.cleanup_expired();
    assert_eq!(manager.subscriber_bindings_len(), 1);
    assert_eq!(manager.get_subscribers("live").len(), 1);
    assert!(manager.get_subscribers("dead").is_empty());
    assert_eq!(manager.subscribers_len(), 1);
}

#[tokio::test]
async fn manager_offline_prunes_only_that_watcher_across_identities() {
    let manager = PresenceManager::new(None);
    let alice = watcher("alice", "pbx.local");
    let bob = watcher("bob", "pbx.local");

    // alice watches bob and carol; bob also watches carol
    manager.add_subscriber("bob", presence_sub(alice.clone(), "a-bob", 3600));
    manager.add_subscriber("carol", presence_sub(alice.clone(), "a-carol", 3600));
    manager.add_subscriber("carol", presence_sub(bob, "b-carol", 3600));
    assert_eq!(manager.subscriber_bindings_len(), 3);

    let pruned = manager
        .handle_locator_event(
            LocatorEvent::Unregistered(Location {
                aor: alice.clone(),
                registered_aor: Some(alice),
                ..Default::default()
            }),
            &EventSource::Local,
        )
        .await;

    assert_eq!(pruned.len(), 2, "alice's two watches must be returned");
    assert_eq!(manager.subscriber_bindings_len(), 1);
    assert_eq!(manager.get_subscribers("carol").len(), 1);
    assert_eq!(
        manager.get_subscribers("carol")[0].dialog_id.call_id,
        "b-carol"
    );
    assert_eq!(manager.get_state("alice").status, PresenceStatus::Offline);
}

#[tokio::test]
async fn manager_webrtc_offline_uses_registered_aor_not_contact_user() {
    let manager = PresenceManager::new(None);
    manager.add_subscriber(
        "1001",
        presence_sub(watcher("1001", "192.168.3.227"), "ws-sub", 3600),
    );

    let pruned = manager
        .handle_locator_event(
            LocatorEvent::Offline(vec![webrtc_location("1001", "vsfbt0co")]),
            &EventSource::Local,
        )
        .await;

    assert_eq!(pruned.len(), 1);
    assert_eq!(pruned[0].call_id, "ws-sub");
    assert_eq!(manager.subscriber_bindings_len(), 0);
    assert_eq!(manager.get_state("1001").status, PresenceStatus::Offline);
    // Only the canonical extension identity is written — never the WebRTC Contact user.
    assert_eq!(manager.states_len(), 1);
}

#[tokio::test]
async fn manager_registered_webrtc_sets_extension_not_contact_user() {
    let manager = PresenceManager::new(None);
    manager
        .handle_locator_event(
            LocatorEvent::Registered(webrtc_location("1001", "vsfbt0co")),
            &EventSource::Local,
        )
        .await;
    assert_eq!(manager.get_state("1001").status, PresenceStatus::Idle);
    assert_eq!(
        manager.get_state("vsfbt0co").status,
        PresenceStatus::Offline,
        "Contact username must not receive Idle"
    );
}

#[tokio::test]
async fn manager_mwi_replace_remove_and_expire_mirror_presence() {
    let manager = PresenceManager::new(None);
    let aor = watcher("1001", "pbx.local");

    // Replace by same watcher.
    assert!(
        manager
            .add_mwi_subscriber("1001", mwi_sub(aor.clone(), "mwi-old", 3600))
            .is_empty()
    );
    let replaced = manager.add_mwi_subscriber("1001", mwi_sub(aor.clone(), "mwi-new", 3600));
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].call_id, "mwi-old");
    assert_eq!(manager.mwi_subscriber_bindings_len(), 1);
    assert_eq!(
        manager.get_mwi_subscribers("1001")[0].dialog_id.call_id,
        "mwi-new"
    );

    // Distinct watchers coexist; expired one is swept without touching the live one.
    manager.add_mwi_subscriber(
        "1001",
        MwiSubscriber {
            aor: watcher("2002", "pbx.local"),
            dialog_id: dialog_id("mwi-expired"),
            account_uri: "sip:1001@pbx.local".into(),
            expires: Instant::now().checked_sub(Duration::from_secs(5)).unwrap(),
        },
    );
    assert_eq!(manager.mwi_subscriber_bindings_len(), 2);
    manager.cleanup_expired_mwi();
    assert_eq!(manager.mwi_subscriber_bindings_len(), 1);
    assert_eq!(
        manager.get_mwi_subscribers("1001")[0].dialog_id.call_id,
        "mwi-new"
    );
    assert_eq!(manager.mwi_subscribers_len(), 1);

    // Watcher-scoped remove clears the bucket.
    let removed = manager.remove_mwi_subscribers_for_watcher("1001");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].call_id, "mwi-new");
    assert_eq!(manager.mwi_subscriber_bindings_len(), 0);
    assert_eq!(manager.mwi_subscribers_len(), 0);
}

#[tokio::test]
async fn manager_offline_also_prunes_mwi_for_watcher() {
    let manager = PresenceManager::new(None);
    let alice = watcher("alice", "pbx.local");
    manager.add_subscriber("bob", presence_sub(alice.clone(), "p1", 3600));
    manager.add_mwi_subscriber("bob", mwi_sub(alice.clone(), "m1", 3600));

    let pruned = manager
        .handle_locator_event(
            LocatorEvent::Offline(vec![Location {
                aor: alice.clone(),
                registered_aor: Some(alice),
                ..Default::default()
            }]),
            &EventSource::Local,
        )
        .await;

    assert_eq!(pruned.len(), 2);
    assert_eq!(manager.subscriber_bindings_len(), 0);
    assert_eq!(manager.mwi_subscriber_bindings_len(), 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. PresenceModule: SUBSCRIBE / NOTIFY fail / locator Offline
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn module_initial_subscribe_sends_ok_for_created_dialog() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let module = PresenceModule::create(server, config).unwrap();
    let (mut tx, endpoint) = create_transaction(subscribe_to(
        "alice",
        "bob",
        "rustpbx.com",
        Some(3600),
        "presence",
    ))
    .await;
    let key = tx.key.clone();

    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    let finished = endpoint
        .finished_transactions
        .get(&key)
        .expect("initial SUBSCRIBE must finish with a response");
    let response = match finished.value().as_ref() {
        Some(SipMessage::Response(response)) => response,
        other => panic!("expected final SUBSCRIBE response, got {other:?}"),
    };
    assert_eq!(response.status_code, StatusCode::OK);
    let response_dialog = DialogId::try_from((response, TransactionRole::Server)).unwrap();
    let subscription = manager.get_subscribers("bob");
    assert_eq!(subscription.len(), 1);
    assert_eq!(response_dialog, subscription[0].dialog_id);
}

#[tokio::test]
async fn module_resubscribe_same_watcher_does_not_accumulate() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let module = PresenceModule::create(server.clone(), config).unwrap();

    for i in 0..5 {
        let _ = i;
        let req = subscribe_to("alice", "bob", "rustpbx.com", Some(3600), "presence");
        // Distinct Call-ID / From-tag each round (create_test_request already
        // randomizes them) to simulate real reconnects.
        let (mut tx, _) = create_transaction(req).await;
        let action = module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx,
                TransactionCookie::default(),
            )
            .await
            .unwrap();
        assert!(matches!(action, ProxyAction::Abort));
    }

    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.subscriber_bindings_len() == 1
        })
        .await,
        "5 reconnect SUBSCRIBEs must collapse to exactly 1 binding, got {}",
        manager.subscriber_bindings_len()
    );
    let subs = manager.get_subscribers("bob");
    assert_eq!(subs.len(), 1);
    assert!(subs[0].aor.to_string().contains("alice@rustpbx.com"));
}

#[tokio::test]
async fn module_expires_zero_unsubscribes_watcher() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let module = PresenceModule::create(server.clone(), config).unwrap();

    let (mut tx_sub, _) = create_transaction(subscribe_to(
        "alice",
        "bob",
        "rustpbx.com",
        Some(3600),
        "presence",
    ))
    .await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_sub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.subscriber_bindings_len() == 1
        })
        .await
    );

    let (mut tx_unsub, _) = create_transaction(subscribe_to(
        "alice",
        "bob",
        "rustpbx.com",
        Some(0),
        "presence",
    ))
    .await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_unsub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.subscriber_bindings_len() == 0
        })
        .await,
        "Expires=0 must clear the watcher's bindings"
    );
}

#[tokio::test]
async fn module_notify_missing_dialog_prunes_dead_binding() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let mut module = PresenceModule::create(server.clone(), config).unwrap();
    module.on_start().await.unwrap();

    // Inject a binding whose dialog was never created in dialog_layer.
    let dead_id = dialog_id("ghost-dialog");
    manager.add_subscriber(
        "bob",
        Subscriber {
            aor: watcher("alice", "rustpbx.com"),
            dialog_id: dead_id.clone(),
            expires: Instant::now() + Duration::from_secs(3600),
        },
    );
    assert_eq!(manager.subscriber_bindings_len(), 1);
    assert!(server.dialog_layer.get_dialog(&dead_id).is_none());

    manager
        .update_state(
            "bob",
            PresenceState {
                status: PresenceStatus::Busy,
                ..Default::default()
            },
            &EventSource::Local,
        )
        .await;

    assert!(
        wait_until(Duration::from_secs(2), || {
            manager.subscriber_bindings_len() == 0
        })
        .await,
        "NOTIFY against a missing dialog must prune the binding (got {})",
        manager.subscriber_bindings_len()
    );
}

#[tokio::test]
async fn module_locator_offline_clears_subscription_and_dialog() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let mut module = PresenceModule::create(server.clone(), config).unwrap();
    module.on_start().await.unwrap();

    let (mut tx_sub, _) = create_transaction(subscribe_to(
        "alice",
        "bob",
        "rustpbx.com",
        Some(3600),
        "presence",
    ))
    .await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_sub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.subscriber_bindings_len() == 1
        })
        .await
    );
    let dialog_id = manager.get_subscribers("bob")[0].dialog_id.clone();
    assert!(
        server.dialog_layer.get_dialog(&dialog_id).is_some(),
        "SUBSCRIBE must register a dialog_layer entry"
    );

    let events = server.locator_events.as_ref().expect("locator_events");
    events
        .send(LocatorEvent::Offline(vec![Location {
            aor: watcher("alice", "rustpbx.com"),
            registered_aor: Some(watcher("alice", "rustpbx.com")),
            ..Default::default()
        }]))
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(2), || {
            manager.subscriber_bindings_len() == 0
                && server.dialog_layer.get_dialog(&dialog_id).is_none()
        })
        .await,
        "Offline must clear subscriber binding AND dialog_layer entry"
    );
    assert_eq!(manager.get_state("alice").status, PresenceStatus::Offline);
}

#[tokio::test]
async fn module_mwi_subscribe_then_offline_clears_binding() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let mut module = PresenceModule::create(server.clone(), config).unwrap();
    module.on_start().await.unwrap();

    let (mut tx_sub, _) = create_transaction(subscribe_to(
        "alice",
        "alice",
        "rustpbx.com",
        Some(3600),
        "message-summary",
    ))
    .await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_sub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.mwi_subscriber_bindings_len() == 1
        })
        .await
    );
    let dialog_id = manager.get_mwi_subscribers("alice")[0].dialog_id.clone();

    server
        .locator_events
        .as_ref()
        .unwrap()
        .send(LocatorEvent::Unregistered(Location {
            aor: watcher("alice", "rustpbx.com"),
            registered_aor: Some(watcher("alice", "rustpbx.com")),
            ..Default::default()
        }))
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(2), || {
            manager.mwi_subscriber_bindings_len() == 0
                && server.dialog_layer.get_dialog(&dialog_id).is_none()
        })
        .await,
        "MWI binding and dialog must be cleared on unregister"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. ClusterEventHub remote Offline + dialog_layer
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cluster_remote_offline_prunes_subscriptions() {
    let (locator_tx, _) = tokio::sync::broadcast::channel(8);
    let manager = Arc::new(PresenceManager::new(None));
    let hub = Arc::new(ClusterEventHub::new(
        locator_tx,
        manager.clone(),
        CancellationToken::new(),
    ));

    let alice = watcher("alice", "pbx.local");
    manager.add_subscriber("bob", presence_sub(alice.clone(), "remote-p", 3600));
    manager.add_mwi_subscriber("bob", mwi_sub(alice.clone(), "remote-m", 3600));

    hub.on_remote_locator_event(
        LocatorEvent::Unregistered(Location {
            aor: alice.clone(),
            registered_aor: Some(alice),
            ..Default::default()
        }),
        EventSource::Remote(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            5060,
        )),
    )
    .await;

    assert_eq!(manager.subscriber_bindings_len(), 0);
    assert_eq!(manager.mwi_subscriber_bindings_len(), 0);
    assert_eq!(manager.get_state("alice").status, PresenceStatus::Offline);
}

#[tokio::test]
async fn cluster_remote_offline_removes_dialog_when_layer_attached() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let module = PresenceModule::create(server.clone(), config).unwrap();

    // Create a real subscription dialog on this node's dialog_layer.
    let (mut tx_sub, _) = create_transaction(subscribe_to(
        "alice",
        "bob",
        "rustpbx.com",
        Some(3600),
        "presence",
    ))
    .await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx_sub,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.subscriber_bindings_len() == 1
        })
        .await
    );
    let dialog_id = manager.get_subscribers("bob")[0].dialog_id.clone();
    assert!(server.dialog_layer.get_dialog(&dialog_id).is_some());

    let (locator_tx, _) = tokio::sync::broadcast::channel(8);
    let hub = ClusterEventHub::new(locator_tx, manager.clone(), CancellationToken::new());
    hub.set_dialog_layer(server.dialog_layer.clone());

    hub.on_remote_locator_event(
        LocatorEvent::Offline(vec![Location {
            aor: watcher("alice", "rustpbx.com"),
            registered_aor: Some(watcher("alice", "rustpbx.com")),
            ..Default::default()
        }]),
        EventSource::Remote(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9)),
            5060,
        )),
    )
    .await;

    assert_eq!(manager.subscriber_bindings_len(), 0);
    assert!(
        server.dialog_layer.get_dialog(&dialog_id).is_none(),
        "remote Offline with set_dialog_layer must free the subscription dialog"
    );
}

#[tokio::test]
async fn cluster_remote_offline_without_dialog_layer_still_prunes_manager() {
    // Guard: missing dialog_layer must not panic and must still clear bindings.
    let (locator_tx, _) = tokio::sync::broadcast::channel(4);
    let manager = Arc::new(PresenceManager::new(None));
    let hub = ClusterEventHub::new(locator_tx, manager.clone(), CancellationToken::new());

    manager.add_subscriber(
        "bob",
        presence_sub(watcher("alice", "pbx.local"), "nolayer", 3600),
    );
    hub.on_remote_locator_event(
        LocatorEvent::Unregistered(Location {
            aor: watcher("alice", "pbx.local"),
            registered_aor: Some(watcher("alice", "pbx.local")),
            ..Default::default()
        }),
        EventSource::Remote(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            5060,
        )),
    )
    .await;
    assert_eq!(manager.subscriber_bindings_len(), 0);
}

#[tokio::test]
async fn reconnect_storm_never_grows_bindings_linearly() {
    // Direct regression for the production log pattern: N reconnects ⇒ N NOTIFYs.
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let mut module = PresenceModule::create(server.clone(), config).unwrap();
    module.on_start().await.unwrap();

    for round in 0..11 {
        if round > 0 {
            server
                .locator_events
                .as_ref()
                .unwrap()
                .send(LocatorEvent::Offline(vec![webrtc_location(
                    "1001", "vsfbt0co",
                )]))
                .unwrap();
            assert!(
                wait_until(Duration::from_secs(2), || {
                    manager.get_state("1001").status == PresenceStatus::Offline
                        && !manager
                            .get_subscribers("1001")
                            .iter()
                            .any(|s| s.aor.user().as_deref() == Some("1001"))
                })
                .await,
                "Offline before reconnect #{round} must clear watcher 1001"
            );
        }

        let (mut tx, _) = create_transaction(subscribe_to(
            "1001",
            "1001",
            "rustpbx.com",
            Some(3600),
            "presence",
        ))
        .await;
        module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx,
                TransactionCookie::default(),
            )
            .await
            .unwrap();

        assert!(
            wait_until(Duration::from_secs(1), || {
                manager
                    .get_subscribers("1001")
                    .iter()
                    .filter(|s| s.aor.user().as_deref() == Some("1001"))
                    .count()
                    == 1
            })
            .await,
            "after reconnect #{round}, watcher 1001 must have exactly 1 binding (got {})",
            manager
                .get_subscribers("1001")
                .iter()
                .filter(|s| s.aor.user().as_deref() == Some("1001"))
                .count()
        );
    }

    assert_eq!(
        manager
            .get_subscribers("1001")
            .iter()
            .filter(|s| s.aor.user().as_deref() == Some("1001"))
            .count(),
        1,
        "after 11 reconnects must still be exactly 1 self-watch binding"
    );
}

#[tokio::test]
async fn module_in_dialog_refresh_keeps_subscription() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let module = PresenceModule::create(server.clone(), config).unwrap();

    // Initial subscription.
    let (mut tx1, _) =
        create_transaction(subscribe_to("alice", "bob", "rustpbx.com", Some(3600), "presence"))
            .await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx1,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(1), || {
            manager.subscriber_bindings_len() == 1
        })
        .await
    );
    let sub = manager.get_subscribers("bob").pop().unwrap();
    let did = sub.dialog_id;

    // In-dialog refresh: same Call-ID, same From tag, To carries the dialog's
    // local tag, fresh Via branch and CSeq (create_test_request randomizes
    // branch; CSeq/Call-ID/tags are overridden below).
    let mut req = subscribe_to("alice", "bob", "rustpbx.com", Some(3600), "presence");
    let from = rsipstack::sip::typed::From {
        display_name: None,
        uri: watcher("alice", "rustpbx.com"),
        params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
            did.remote_tag.clone(),
        ))],
    };
    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: watcher("bob", "rustpbx.com"),
        params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
            did.local_tag.clone(),
        ))],
    };
    req.headers.retain(|h| {
        !matches!(
            h,
            rsipstack::sip::Header::CallId(_)
                | rsipstack::sip::Header::From(_)
                | rsipstack::sip::Header::To(_)
        )
    });
    req.headers.push(rsipstack::sip::Header::CallId(
        rsipstack::sip::headers::CallId::new(did.call_id.clone()),
    ));
    req.headers.push(rsipstack::sip::Header::From(from.into()));
    req.headers.push(rsipstack::sip::Header::To(to.into()));

    let (mut tx2, _) = create_transaction(req).await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx2,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Give any misbehaving guard task time to run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let subs = manager.get_subscribers("bob");
    assert_eq!(
        subs.len(),
        1,
        "in-dialog refresh must keep exactly 1 binding, got {}: {:?}",
        subs.len(),
        subs.iter().map(|s| &s.dialog_id).collect::<Vec<_>>()
    );
    assert_eq!(subs[0].dialog_id, did, "refresh must keep the same dialog");
}
