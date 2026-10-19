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

use super::common::{
    create_test_request, create_test_server, create_test_server_with_config, create_transaction,
};
use crate::call::{Location, TransactionCookie};
use crate::config::ProxyConfig;
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
            LocatorEvent::Offline(
                vec![webrtc_location("1001", "vsfbt0co")],
                crate::proxy::locator::LocatorOfflineReason::TransportClosed,
            ),
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
            LocatorEvent::Offline(
                vec![Location {
                    aor: alice.clone(),
                    registered_aor: Some(alice),
                    ..Default::default()
                }],
                crate::proxy::locator::LocatorOfflineReason::TransportClosed,
            ),
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

/// The 200 OK must echo the granted expiry so clients that only parse the
/// response `Expires` header (instead of the NOTIFY `Subscription-State`)
/// schedule refreshes at the right cadence.
#[tokio::test]
async fn module_subscribe_200_ok_echoes_granted_expires() {
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
    let echoed = response
        .headers
        .iter()
        .find_map(|h| match h {
            rsipstack::sip::Header::Expires(e) => Some(e.value().to_string()),
            _ => None,
        })
        .expect("SUBSCRIBE 200 OK must carry an Expires header");
    assert_eq!(echoed, "3600");

    // The stored subscription lifetime matches the advertised value, so the
    // NOTIFY `Subscription-State: active;expires=` stays consistent.
    let subs = manager.get_subscribers("bob");
    assert_eq!(subs.len(), 1);
    let left = subs[0]
        .expires
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or_default();
    assert!(
        left > Duration::from_secs(3500),
        "subscriber expiry should track the granted 3600s, got {left:?}"
    );
}

/// Requested expiries above `max_subscription_expires` are clamped and the
/// clamped value is what gets advertised.
#[tokio::test]
async fn module_subscribe_expires_capped_by_max_subscription_expires() {
    let mut cfg = ProxyConfig::default();
    cfg.max_subscription_expires = Some(120);
    let (server, config) = create_test_server_with_config(cfg).await;
    let manager = server.presence_manager.clone();
    let module = PresenceModule::create(server, config).unwrap();
    let (mut tx, endpoint) = create_transaction(subscribe_to(
        "alice",
        "bob",
        "rustpbx.com",
        Some(99999),
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
    let echoed = response
        .headers
        .iter()
        .find_map(|h| match h {
            rsipstack::sip::Header::Expires(e) => Some(e.value().to_string()),
            _ => None,
        })
        .expect("SUBSCRIBE 200 OK must carry an Expires header");
    assert_eq!(echoed, "120");
    let subs = manager.get_subscribers("bob");
    assert_eq!(subs.len(), 1);
    let left = subs[0]
        .expires
        .checked_duration_since(std::time::Instant::now())
        .unwrap_or_default();
    assert!(
        left > Duration::from_secs(100) && left <= Duration::from_secs(120),
        "subscriber expiry should track the clamped 120s, got {left:?}"
    );
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
                reason: None,
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
        .send(LocatorEvent::Offline(
            vec![Location {
                aor: watcher("alice", "rustpbx.com"),
                registered_aor: Some(watcher("alice", "rustpbx.com")),
                ..Default::default()
            }],
            crate::proxy::locator::LocatorOfflineReason::TransportClosed,
        ))
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
        LocatorEvent::Offline(
            vec![Location {
                aor: watcher("alice", "rustpbx.com"),
                registered_aor: Some(watcher("alice", "rustpbx.com")),
                ..Default::default()
            }],
            crate::proxy::locator::LocatorOfflineReason::TransportClosed,
        ),
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
                .send(LocatorEvent::Offline(
                    vec![webrtc_location("1001", "vsfbt0co")],
                    crate::proxy::locator::LocatorOfflineReason::TransportClosed,
                ))
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
    let (mut tx1, _) = create_transaction(subscribe_to(
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

// Real SIP notifications: a counter before dialog lookup is not delivery evidence.
struct PresenceWireUa {
    server: crate::proxy::server::SipServerRef,
    socket: Arc<tokio::net::UdpSocket>,
    responses: tokio::sync::mpsc::UnboundedReceiver<String>,
    notifications: tokio::sync::mpsc::UnboundedReceiver<String>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    address: SocketAddr,
}

impl Drop for PresenceWireUa {
    fn drop(&mut self) {
        self.server.cancel_token.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl PresenceWireUa {
    async fn start() -> Self {
        let port = portpicker::pick_unused_port().unwrap();
        let config = Arc::new(ProxyConfig {
            addr: "127.0.0.1".into(),
            udp_port: Some(port),
            modules: Some(vec!["presence".into()]),
            ensure_user: Some(false),
            ..Default::default()
        });
        let server = Arc::new(
            crate::proxy::server::SipServerBuilder::new(config)
                .with_user_backend(Box::new(crate::proxy::user::MemoryUserBackend::new(None)))
                .register_module("presence", PresenceModule::create)
                .build()
                .await
                .unwrap(),
        );
        let server_ref = server.get_inner();
        let serve = crate::utils::spawn(async move {
            server.serve().await.unwrap();
        });
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (response_tx, responses) = tokio::sync::mpsc::unbounded_channel();
        let (notify_tx, notifications) = tokio::sync::mpsc::unbounded_channel();
        let receiver = socket.clone();
        let receive = crate::utils::spawn(async move {
            let mut buffer = [0; 16384];
            loop {
                let (length, source) = receiver.recv_from(&mut buffer).await.unwrap();
                let message = String::from_utf8(buffer[..length].to_vec()).unwrap();
                if message.starts_with("NOTIFY ") {
                    // Echo transaction headers so the actual server NOTIFY completes.
                    let headers = message
                        .split("\r\n")
                        .filter(|line| {
                            line.split_once(':').is_some_and(|(name, _)| {
                                ["via", "from", "to", "call-id", "cseq"]
                                    .contains(&name.to_ascii_lowercase().as_str())
                            })
                        })
                        .collect::<Vec<_>>()
                        .join("\r\n");
                    let reply = format!("SIP/2.0 200 OK\r\n{headers}\r\nContent-Length: 0\r\n\r\n");
                    receiver.send_to(reply.as_bytes(), source).await.unwrap();
                    notify_tx.send(message).unwrap();
                } else if message.starts_with("SIP/2.0 ") {
                    response_tx.send(message).unwrap();
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        Self {
            server: server_ref,
            socket,
            responses,
            notifications,
            tasks: vec![serve, receive],
            address: ([127, 0, 0, 1], port).into(),
        }
    }

    async fn request(&mut self, method: &str, from: &str, to: &str, body: &str) {
        let local = self.socket.local_addr().unwrap();
        let id = rsipstack::transaction::random_text(12);
        let request = format!(
            "{method} sip:{to}@{server} SIP/2.0\r\nVia: SIP/2.0/UDP {local};branch=z9hG4bK{id}\r\nFrom: <sip:{from}@127.0.0.1>;tag={id}\r\nTo: <sip:{to}@127.0.0.1>\r\nCall-ID: {id}\r\nCSeq: 1 {method}\r\nContact: <sip:{from}@{local}>\r\nMax-Forwards: 70\r\nEvent: presence\r\nExpires: 3600\r\nContent-Type: application/pidf+xml\r\nContent-Length: {length}\r\n\r\n{body}",
            server = self.address,
            length = body.len(),
        );
        self.socket
            .send_to(request.as_bytes(), self.address)
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), self.responses.recv())
            .await
            .expect("SIP response")
            .unwrap();
        assert!(
            response.starts_with("SIP/2.0 200 "),
            "{method} must be accepted: {response}"
        );
    }

    async fn notify(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(2), self.notifications.recv())
            .await
            .expect("actual NOTIFY must reach the UDP watcher")
            .unwrap()
    }
}

#[tokio::test]
async fn oc_external_watcher_receives_initial_offline_and_published_closed() {
    let mut ua = PresenceWireUa::start().await;
    ua.request("SUBSCRIBE", "alice", "bob", "").await;
    assert!(ua.notify().await.contains("<basic>closed</basic>"));

    ua.request("PUBLISH", "bob", "bob",
        "<presence xmlns=\"urn:ietf:params:xml:ns:pidf\"><tuple id=\"p\"><status><basic>open</basic></status></tuple></presence>").await;
    assert!(ua.notify().await.contains("<basic>open</basic>"));
    ua.request("PUBLISH", "bob", "bob",
        "<presence xmlns=\"urn:ietf:params:xml:ns:pidf\"><tuple id=\"p\"><status><basic>closed</basic></status></tuple></presence>").await;
    assert!(ua.notify().await.contains("<basic>closed</basic>"));
    assert!(
        wait_until(Duration::from_secs(1), || ua
            .server
            .presence_manager
            .notifies_sent()
            == 3)
        .await
    );
}

#[tokio::test]
async fn oc_self_reconnect_suppresses_stale_offline_but_delivers_idle() {
    let mut ua = PresenceWireUa::start().await;
    ua.server
        .presence_manager
        .update_state(
            "alice",
            PresenceState {
                reason: None,
                offline_reason: Some("offline".into()),
                ..Default::default()
            },
            &EventSource::Local,
        )
        .await;
    ua.request("SUBSCRIBE", "alice", "alice", "").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), ua.notifications.recv())
            .await
            .is_err(),
        "stale self Offline must not force logout during reconnect"
    );
    ua.server
        .presence_manager
        .handle_locator_event(
            LocatorEvent::Registered(Location {
                registered_aor: Some("sip:alice@127.0.0.1".parse().unwrap()),
                ..Default::default()
            }),
            &EventSource::Local,
        )
        .await;
    assert!(ua.notify().await.contains("<note>idle</note>"));
    assert!(
        wait_until(Duration::from_secs(1), || ua
            .server
            .presence_manager
            .notifies_sent()
            == 1)
        .await
    );
    ua.server
        .presence_manager
        .update_state(
            "alice",
            PresenceState {
                reason: None,
                status: PresenceStatus::Offline,
                offline_reason: Some("manual".into()),
                ..Default::default()
            },
            &EventSource::Local,
        )
        .await;
    assert!(
        ua.notify().await.contains("<basic>closed</basic>"),
        "an explicit server logout must still notify the subscriber itself"
    );
}

#[tokio::test]
async fn oc_invalid_dialog_is_not_counted_as_sent() {
    let (server, config) = create_test_server().await;
    let manager = server.presence_manager.clone();
    let mut module = PresenceModule::create(server, config).unwrap();
    module.on_start().await.unwrap();
    manager.add_subscriber(
        "bob",
        presence_sub(watcher("alice", "rustpbx.com"), "missing", 3600),
    );
    manager
        .update_state(
            "bob",
            PresenceState {
                reason: None,
                status: PresenceStatus::Idle,
                ..Default::default()
            },
            &EventSource::Local,
        )
        .await;
    assert!(
        wait_until(Duration::from_secs(1), || manager
            .get_subscribers("bob")
            .is_empty())
        .await,
        "failed delivery must prune the invalid dialog"
    );
    assert_eq!(
        manager.notifies_sent(),
        0,
        "missing dialog never delivered a NOTIFY"
    );
    assert!(manager.last_notified().is_none());
}

#[tokio::test]
async fn oc_publish_offline_survives_register_keepalive() {
    let mut ua = PresenceWireUa::start().await;
    ua.request("PUBLISH", "bob", "bob",
        "<presence xmlns=\"urn:ietf:params:xml:ns:pidf\"><tuple id=\"p\"><status><basic>closed</basic></status></tuple></presence>").await;
    ua.server
        .presence_manager
        .handle_locator_event(
            LocatorEvent::Refreshed(Location {
                registered_aor: Some("sip:bob@127.0.0.1".parse().unwrap()),
                ..Default::default()
            }),
            &EventSource::Local,
        )
        .await;
    assert_eq!(
        ua.server.presence_manager.get_state("bob").status,
        PresenceStatus::Offline,
        "a SIP keepalive must not undo the user's PUBLISH logout"
    );
}
