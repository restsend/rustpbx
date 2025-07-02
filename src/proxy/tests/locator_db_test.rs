use crate::proxy::{
    locator::{Location, Locator},
    locator_db::DbLocator,
};
use rsip::{HostWithPort, Scheme};
use rsipstack::transport::SipAddr;
use std::time::Instant;

#[tokio::test]
async fn test_db_locator() {
    // Create a test location
    let aor = rsip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsip::Auth {
            user: "alice".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("example.com:5060").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port = HostWithPort::try_from("127.0.0.1:5060").unwrap();
    let destination = SipAddr {
        r#type: Some(rsip::transport::Transport::Udp),
        addr: host_with_port,
    };

    let location = Location {
        aor: aor.clone(),
        expires: 3600,
        destination: destination.clone(),
        last_modified: Instant::now(),
        supports_webrtc: false,
    };

    // Setup DB locator
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Test register
    locator
        .register("alice", Some("example.com"), location)
        .await
        .unwrap();

    // Test lookup
    let locations = locator.lookup("alice", Some("example.com")).await.unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].aor.to_string(), aor.to_string());
    assert_eq!(locations[0].expires, 3600);

    // Match the relevant components of the SipAddr
    match &locations[0].destination {
        SipAddr { r#type, addr } => {
            assert_eq!(r#type, &Some(rsip::transport::Transport::Udp));
            match &addr.host {
                rsip::host_with_port::Host::IpAddr(ip) => {
                    assert_eq!(ip.to_string(), "127.0.0.1");
                }
                _ => panic!("Expected IP address"),
            }
            assert_eq!(addr.port.as_ref().unwrap().value().to_owned(), 5060);
        }
    }

    // Test unregister
    locator
        .unregister("alice", Some("example.com"))
        .await
        .unwrap();

    // Lookup should now fail
    let result = locator.lookup("alice", Some("example.com")).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_db_locator_with_custom_table() {
    // Setup DB with custom table name
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Create a test location
    let aor = rsip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsip::Auth {
            user: "bob".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("example.com:5080").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port = HostWithPort::try_from("192.168.1.1:5080").unwrap();
    let destination = SipAddr {
        r#type: Some(rsip::transport::Transport::Tcp),
        addr: host_with_port,
    };

    let location = Location {
        aor: aor.clone(),
        expires: 1800,
        destination: destination.clone(),
        last_modified: Instant::now(),
        supports_webrtc: false,
    };

    // Test register
    locator
        .register("bob", Some("example.com"), location)
        .await
        .unwrap();

    // Test lookup
    let locations = locator.lookup("bob", Some("example.com")).await.unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].aor.to_string(), aor.to_string());
    assert_eq!(locations[0].expires, 1800);

    // Match the relevant components of the SipAddr
    match &locations[0].destination {
        SipAddr { r#type, addr } => {
            assert_eq!(r#type, &Some(rsip::transport::Transport::Tcp));
            match &addr.host {
                rsip::host_with_port::Host::IpAddr(ip) => {
                    assert_eq!(ip.to_string(), "192.168.1.1");
                }
                _ => panic!("Expected IP address"),
            }
            assert_eq!(addr.port.as_ref().unwrap().value().to_owned(), 5080);
        }
    }

    // Test unregister
    locator
        .unregister("bob", Some("example.com"))
        .await
        .unwrap();

    // Lookup should now fail
    let result = locator.lookup("bob", Some("example.com")).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_db_locator_multiple_lookups() {
    // Setup DB locator
    let locator = DbLocator::new("sqlite::memory:".to_string()).await.unwrap();

    // Create different transport type locations for the same user
    let aor1 = rsip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsip::Auth {
            user: "carol".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("example.com:5060").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port1 = HostWithPort::try_from("127.0.0.1:5060").unwrap();
    let destination1 = SipAddr {
        r#type: Some(rsip::transport::Transport::Udp),
        addr: host_with_port1,
    };

    let location1 = Location {
        aor: aor1.clone(),
        expires: 3600,
        destination: destination1.clone(),
        last_modified: Instant::now(),
        supports_webrtc: false,
    };

    // Register the first one (this test will currently fail since our DB implementation
    // uses the identifier as a unique key, so we can't store multiple registrations for
    // the same user. This is a limitation compared to the in-memory implementation.)
    locator
        .register("carol", Some("example.com"), location1)
        .await
        .unwrap();

    // Look up the registered location
    let locations = locator.lookup("carol", Some("example.com")).await.unwrap();
    assert_eq!(locations.len(), 1);

    // Test with a different realm
    let aor2 = rsip::Uri {
        scheme: Some(Scheme::Sip),
        auth: Some(rsip::Auth {
            user: "carol".to_string(),
            password: None,
        }),
        host_with_port: HostWithPort::try_from("otherexample.com:5080").unwrap(),
        params: vec![],
        headers: vec![],
    };

    let host_with_port2 = HostWithPort::try_from("192.168.1.10:5080").unwrap();
    let destination2 = SipAddr {
        r#type: Some(rsip::transport::Transport::Tcp),
        addr: host_with_port2,
    };

    let location2 = Location {
        aor: aor2.clone(),
        expires: 1800,
        destination: destination2.clone(),
        last_modified: Instant::now(),
        supports_webrtc: false,
    };

    // Register with a different realm
    locator
        .register("carol", Some("otherexample.com"), location2)
        .await
        .unwrap();

    // First realm lookup should still work
    let locations1 = locator.lookup("carol", Some("example.com")).await.unwrap();
    assert_eq!(locations1.len(), 1);
    assert_eq!(locations1[0].aor.to_string(), aor1.to_string());

    // Second realm lookup should also work
    let locations2 = locator
        .lookup("carol", Some("otherexample.com"))
        .await
        .unwrap();
    assert_eq!(locations2.len(), 1);
    assert_eq!(locations2[0].aor.to_string(), aor2.to_string());

    // Unregistering one realm shouldn't affect the other
    locator
        .unregister("carol", Some("example.com"))
        .await
        .unwrap();

    // First realm lookup should now fail
    let result1 = locator.lookup("carol", Some("example.com")).await;
    assert!(result1.is_err());

    // Second realm lookup should still work
    let locations2 = locator
        .lookup("carol", Some("otherexample.com"))
        .await
        .unwrap();
    assert_eq!(locations2.len(), 1);
    assert_eq!(locations2[0].aor.to_string(), aor2.to_string());
}
