use crate::{
    call::Location,
    proxy::{locator::Locator, locator_db::DbLocator},
};
use rsipstack::sip::{HostWithPort, Scheme};
use rsipstack::transport::SipAddr;
use std::sync::Arc;
use std::time::Instant;

#[tokio::test]
async fn test_db_locator() {
    // Create a test location
    let aor = rsipstack::sip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "alice".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("rustpbx.com").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port = HostWithPort::try_from("127.0.0.1:5060").unwrap();
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: host_with_port,
    };

    let location = Location {
        aor: aor.clone(),
        expires: 3600,
        destination: Some(destination.clone()),
        last_modified: Some(Instant::now()),
        ..Default::default()
    };

    // Setup DB locator
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Test register
    locator
        .register("alice", Some("rustpbx.com"), location)
        .await
        .unwrap();

    // Test lookup
    let locations = locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].aor.to_string(), aor.to_string());
    assert_eq!(locations[0].expires, 3600);

    // Match the relevant components of the SipAddr
    match &locations[0].destination {
        Some(SipAddr { r#type, addr }) => {
            assert_eq!(r#type, &Some(rsipstack::sip::transport::Transport::Udp));
            match &addr.host {
                rsipstack::sip::Host::IpAddr(ip) => {
                    assert_eq!(ip.to_string(), "127.0.0.1");
                }
                _ => panic!("Expected IP address"),
            }
            assert_eq!(addr.port.as_ref().unwrap().value().to_owned(), 5060);
        }
        None => panic!("Expected destination to be Some"),
    }

    // Test unregister
    locator
        .unregister("alice", Some("rustpbx.com"))
        .await
        .unwrap();

    // Lookup should now return none (empty) or an error, both acceptable
    let result = locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await;
    if let Ok(v) = result { assert!(v.is_empty(), "Expected no locations after unregister") }
}

#[tokio::test]
async fn test_db_locator_with_custom_table() {
    // Setup DB with custom table name
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Create a test location
    let aor = rsipstack::sip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "bob".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("rustpbx.com:5080").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port = HostWithPort::try_from("192.168.1.1:5080").unwrap();
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Tcp),
        addr: host_with_port,
    };

    let location = Location {
        aor: aor.clone(),
        expires: 1800,
        destination: Some(destination.clone()),
        last_modified: Some(Instant::now()),
        ..Default::default()
    };

    // Test register
    locator
        .register("bob", Some("rustpbx.com"), location)
        .await
        .unwrap();

    // Test lookup
    let locations = locator
        .lookup(&"sip:bob@rustpbx.com:5080".try_into().expect("invalid uri"))
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].aor.to_string(), aor.to_string());
    assert_eq!(locations[0].expires, 1800);

    // Match the relevant components of the SipAddr
    match &locations[0].destination {
        Some(SipAddr { r#type, addr }) => {
            assert_eq!(r#type, &Some(rsipstack::sip::transport::Transport::Tcp));
            match &addr.host {
                rsipstack::sip::Host::IpAddr(ip) => {
                    assert_eq!(ip.to_string(), "192.168.1.1");
                }
                _ => panic!("Expected IP address"),
            }
            assert_eq!(addr.port.as_ref().unwrap().value().to_owned(), 5080);
        }
        None => panic!("Expected destination to be Some"),
    }

    // Test unregister
    locator
        .unregister("bob", Some("rustpbx.com"))
        .await
        .unwrap();

    // Lookup should now return none (empty) or an error, both acceptable
    let result = locator
        .lookup(&"sip:bob@rustpbx.com".try_into().expect("invalid uri"))
        .await;
    if let Ok(v) = result { assert!(v.is_empty(), "Expected no locations after unregister") }
}

#[tokio::test]
async fn test_db_locator_multiple_lookups() {
    // Setup DB locator
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Create different transport type locations for the same user
    let aor1 = rsipstack::sip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "carol".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("rustpbx.com").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port1 = HostWithPort::try_from("127.0.0.1:5060").unwrap();
    let destination1 = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: host_with_port1,
    };

    let location1 = Location {
        aor: aor1.clone(),
        expires: 3600,
        destination: Some(destination1.clone()),
        last_modified: Some(Instant::now()),
        ..Default::default()
    };

    // Register the first one (this test will currently fail since our DB implementation
    // uses the identifier as a unique key, so we can't store multiple registrations for
    // the same user. This is a limitation compared to the in-memory implementation.)
    locator
        .register("carol", Some("rustpbx.com"), location1)
        .await
        .unwrap();

    // Look up the registered location
    let locations = locator
        .lookup(&"sip:carol@rustpbx.com".try_into().expect("invalid uri"))
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);

    // Test with a different realm
    let aor2 = rsipstack::sip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "carol".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("otherrustpbx.com:5080").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port2 = HostWithPort::try_from("192.168.1.10:5080").unwrap();
    let destination2 = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Tcp),
        addr: host_with_port2,
    };

    let location2 = Location {
        aor: aor2.clone(),
        expires: 1800,
        destination: Some(destination2.clone()),
        last_modified: Some(Instant::now()),
        ..Default::default()
    };

    // Register with a different realm
    locator
        .register("carol", Some("otherrustpbx.com"), location2)
        .await
        .unwrap();

    // First realm lookup should still work
    let locations1 = locator
        .lookup(&"sip:carol@rustpbx.com".try_into().expect("invalid uri"))
        .await
        .unwrap();
    assert_eq!(locations1.len(), 1);
    assert_eq!(locations1[0].aor.to_string(), aor1.to_string());

    // Second realm lookup should also work
    let locations2 = locator
        .lookup(
            &"sip:carol@otherrustpbx.com:5080"
                .try_into()
                .expect("invalid uri"),
        )
        .await
        .unwrap();
    assert_eq!(locations2.len(), 1);
    assert_eq!(locations2[0].aor.to_string(), aor2.to_string());

    // Unregistering one realm shouldn't affect the other
    locator
        .unregister("carol", Some("rustpbx.com"))
        .await
        .unwrap();

    // First realm lookup should now be none (empty) or an error, both acceptable
    let result1 = locator
        .lookup(&"sip:carol@rustpbx.com".try_into().expect("invalid uri"))
        .await;
    if let Ok(v) = result1 { assert!(
        v.is_empty(),
        "Expected no locations after unregister for realm rustpbx.com"
    ) }

    // Second realm lookup should still work
    let locations2 = locator
        .lookup(
            &"sip:carol@otherrustpbx.com:5080"
                .try_into()
                .expect("invalid uri"),
        )
        .await
        .unwrap();
    assert_eq!(locations2.len(), 1);
    assert_eq!(locations2[0].aor.to_string(), aor2.to_string());
}

#[tokio::test]
async fn test_db_locator_localhost_alias() {
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    let aor = rsipstack::sip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "dave".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("192.168.3.181").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let destination = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: HostWithPort::try_from("192.168.3.181:5060").unwrap(),
    };

    locator
        .register(
            "dave",
            Some("192.168.3.181"),
            Location {
                aor: aor.clone(),
                expires: 3600,
                destination: Some(destination),
                last_modified: Some(Instant::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let locations = locator
        .lookup(&"sip:dave@localhost".try_into().expect("invalid uri"))
        .await
        .unwrap();

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].aor.to_string(), aor.to_string());
}

#[tokio::test]
async fn test_db_locator_home_proxy_crud() {
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    let aor = rsipstack::sip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "alice".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("rustpbx.com").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let destination = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
    };

    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Tcp),
        addr: HostWithPort::try_from("10.0.0.1:5060").unwrap(),
    };

    // Register with home_proxy
    locator
        .register(
            "alice",
            Some("rustpbx.com"),
            Location {
                aor: aor.clone(),
                expires: 3600,
                destination: Some(destination.clone()),
                home_proxy: Some(home_proxy.clone()),
                last_modified: Some(Instant::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Lookup should preserve home_proxy
    let locations = locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].destination, Some(destination.clone()));
    assert_eq!(locations[0].home_proxy, Some(home_proxy));

    // Update: change home_proxy
    let new_home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: HostWithPort::try_from("10.0.0.2:5060").unwrap(),
    };
    locator
        .register(
            "alice",
            Some("rustpbx.com"),
            Location {
                aor: aor.clone(),
                expires: 3600,
                destination: Some(destination.clone()),
                home_proxy: Some(new_home_proxy.clone()),
                last_modified: Some(Instant::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let locations = locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].home_proxy, Some(new_home_proxy));

    // Unregister and verify cleanup
    locator.unregister("alice", Some("rustpbx.com")).await.unwrap();
    let result = locator
        .lookup(&"sip:alice@rustpbx.com".try_into().expect("invalid uri"))
        .await;
    if let Ok(v) = result {
        assert!(v.is_empty(), "Expected no locations after unregister");
    }
}

#[tokio::test]
async fn test_db_locator_rewrites_legacy_registered_aor_contact_to_canonical_aor() {
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    let contact_uri: rsipstack::sip::Uri = "sip:lp@172.25.52.29:51003;transport=UDP"
        .try_into()
        .unwrap();
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: HostWithPort::try_from("172.25.52.29:51003").unwrap(),
    };
    let home_proxy = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: HostWithPort::try_from("10.145.213.70:8060").unwrap(),
    };

    // Simulate legacy record where registered_aor is incorrectly equal to contact AoR
    locator
        .register(
            "lp",
            Some("localhost"),
            Location {
                aor: contact_uri.clone(),
                registered_aor: Some(contact_uri),
                destination: Some(destination),
                home_proxy: Some(home_proxy),
                expires: 3600,
                last_modified: Some(Instant::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let results = locator
        .lookup(&"sip:lp@localhost".try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(results.len(), 1);

    let canonical = results[0].registered_aor.as_ref().unwrap().to_string();
    assert_eq!(canonical, "sip:lp@localhost");
}

/// Regression test: when a user registers with a private IP (realm gets normalized to
/// localhost by the DB locator) and is later looked up by a configured domain name,
/// the record must still be returned.  Previously a `uri_matches` guard at the end of
/// `DbLocator::lookup` rejected the result because the AoR host (IP) did not match the
/// lookup host (domain).
#[tokio::test]
async fn test_db_locator_lookup_by_domain_when_registered_with_ip() {
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Simulate the realm_checker that SipServer installs — it recognises the
    // configured domain as a local realm so lookups get normalised to "localhost".
    locator.set_realm_checker(Arc::new(|realm: &str| {
        let realm = realm.to_string();
        Box::pin(async move {
            realm == "kefutest.xiaojukeji.com" || crate::proxy::locator::is_local_realm(&realm)
        })
    }));

    let contact_aor: rsipstack::sip::Uri = "sip:bp@172.28.47.170:57491".try_into().unwrap();
    let destination = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: HostWithPort::try_from("172.28.47.170:57491").unwrap(),
    };

    // Register with a private-IP realm — the DB locator normalises this to "localhost".
    locator
        .register(
            "bp",
            Some("172.28.47.170"),
            Location {
                aor: contact_aor.clone(),
                destination: Some(destination),
                expires: 3600,
                last_modified: Some(Instant::now()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Lookup by the configured domain name must return the record.
    let results = locator
        .lookup(&"sip:bp@kefutest.xiaojukeji.com".try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "Should find record when querying by domain after registering with IP"
    );
    assert_eq!(results[0].aor.to_string(), contact_aor.to_string());

    // Sanity: looking up a different user on the same domain returns nothing.
    let empty = locator
        .lookup(&"sip:wp@kefutest.xiaojukeji.com".try_into().unwrap())
        .await
        .unwrap();
    assert!(empty.is_empty(), "Different user should not match");
}
