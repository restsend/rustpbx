use super::common::{
    create_acl_request, create_test_request, create_test_server_with_config, create_transaction,
};
use crate::call::{TransactionCookie, TrunkContext};
use crate::config::ProxyConfig;
use crate::proxy::acl::AclModule;
use crate::proxy::routing::{TrunkConfig, TrunkDirection};
use crate::proxy::{ProxyAction, ProxyModule};
use rsipstack::sip::Header;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_acl_module_allow_normal_request() {
    // Create config
    let config = Arc::new(ProxyConfig::default());

    // Create a normal request
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "127.0.0.1");

    // Create the_acl module
    let module = AclModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test_acl module
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should continue since it's a normal request
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn trusted_proxy_uses_second_separate_via_for_inbound_trunk() {
    let mut config = ProxyConfig {
        trusted_proxies: vec!["127.0.0.1".to_string()],
        acl_rules: Some(vec!["deny all".to_string()]),
        generated_dir: format!(
            "target/test-generated/acl-trusted-proxy-via-{}",
            std::process::id()
        ),
        ..Default::default()
    };
    config.trunks.insert(
        "forwarded-carrier".to_string(),
        TrunkConfig {
            id: Some(401),
            direction: Some(TrunkDirection::Inbound),
            inbound_hosts: vec!["198.51.100.25".to_string()],
            ..Default::default()
        },
    );
    let (server, config) = create_test_server_with_config(config).await;
    let module = AclModule::create(server, config).unwrap();

    let mut request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    request.headers.retain(|header| !matches!(header, Header::Via(_)));
    request.headers.push_front(Header::Via(
        rsipstack::sip::headers::Via::new(
            "SIP/2.0/UDP 10.0.0.10:5060;branch=z9hG4bK-origin;received=198.51.100.25",
        ),
    ));
    request.headers.push_front(Header::Via(
        rsipstack::sip::headers::Via::new(
            "SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-proxy",
        ),
    ));
    let (mut tx, _) = create_transaction(request).await;
    let cookie = TransactionCookie::default();

    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx, cookie.clone())
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Continue));
    let trunk = cookie
        .get_extension::<TrunkContext>()
        .expect("second Via received IP should identify the inbound trunk");
    assert_eq!(trunk.id, Some(401));
    assert_eq!(trunk.name, "forwarded-carrier");
}

#[tokio::test]
async fn untrusted_socket_ignores_forwarded_via_for_inbound_trunk() {
    let mut config = ProxyConfig {
        trusted_proxies: vec!["203.0.113.10".to_string()],
        acl_rules: Some(vec!["deny all".to_string()]),
        generated_dir: format!(
            "target/test-generated/acl-untrusted-proxy-via-{}",
            std::process::id()
        ),
        ..Default::default()
    };
    config.trunks.insert(
        "forged-carrier".to_string(),
        TrunkConfig {
            id: Some(402),
            direction: Some(TrunkDirection::Inbound),
            inbound_hosts: vec!["198.51.100.25".to_string()],
            ..Default::default()
        },
    );
    let (server, config) = create_test_server_with_config(config).await;
    let module = AclModule::create(server, config).unwrap();

    let mut request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    request.headers.retain(|header| !matches!(header, Header::Via(_)));
    request.headers.push_front(Header::Via(
        rsipstack::sip::headers::Via::new(
            "SIP/2.0/UDP 10.0.0.10:5060;branch=z9hG4bK-forged;received=198.51.100.25",
        ),
    ));
    request.headers.push_front(Header::Via(
        rsipstack::sip::headers::Via::new(
            "SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-direct",
        ),
    ));
    let (mut tx, _) = create_transaction(request).await;
    let cookie = TransactionCookie::default();

    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx, cookie.clone())
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Abort));
    assert!(cookie.get_extension::<TrunkContext>().is_none());
}

#[tokio::test]
async fn test_acl_invalid_rules() {
    let config = ProxyConfig {
        acl_rules: Some(vec!["invalid_rule".to_string(), "allow all".to_string()]),
        ..Default::default()
    };
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Should use default rules when invalid rules are present
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "127.0.0.1");
    let (mut tx, _) = create_transaction(request).await;
    assert!(matches!(
        module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx,
                TransactionCookie::default()
            )
            .await
            .unwrap(),
        ProxyAction::Continue
    ));
}

#[tokio::test]
async fn test_acl_ipv6() {
    // Test IPv6 support directly using the AclModule unit tests
    let config = ProxyConfig {
        acl_rules: Some(vec![
            "allow 2001:db8::/32".to_string(),
            "deny all".to_string(),
        ]),
        ..Default::default()
    };
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Test allowed IPv6 directly
    let ipv6_allowed: std::net::IpAddr = "2001:db8::1".parse().unwrap();
    assert!(module.is_ip_allowed(&ipv6_allowed).await);

    // Test denied IPv6 directly
    let ipv6_denied: std::net::IpAddr = "2001:db9::1".parse().unwrap();
    assert!(!module.is_ip_allowed(&ipv6_denied).await);
}

// ── URI normalization integration tests ─────────────────────────

fn create_request_with_from_uri(from_uri: &str) -> rsipstack::sip::Request {
    let host_with_port = rsipstack::sip::HostWithPort {
        host: "127.0.0.1".parse().unwrap(),
        port: Some(5060.into()),
    };

    let uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "alice".to_string(),
            password: None,
        }),
        host_with_port: host_with_port.clone(),
        params: vec![],
        headers: vec![],
    };

    let from = rsipstack::sip::typed::From {
        display_name: None,
        uri: from_uri.parse().unwrap(),
        params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
            "abc123",
        ))],
    };

    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    let via =
        rsipstack::sip::headers::Via::new("SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-testbranch");

    let call_id = rsipstack::sip::headers::CallId::new("testcallid");
    let cseq = rsipstack::sip::headers::typed::CSeq {
        seq: 1u32,
        method: rsipstack::sip::Method::Invite,
    };

    let contact_uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: "alice".to_string(),
            password: None,
        }),
        host_with_port,
        params: vec![],
        headers: vec![],
    };

    let contact = rsipstack::sip::typed::Contact {
        display_name: None,
        uri: contact_uri,
        params: vec![],
    };

    rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri,
        version: rsipstack::sip::Version::V2,
        headers: vec![
            from.into(),
            to.into(),
            via.into(),
            call_id.into(),
            cseq.into(),
            contact.into(),
        ]
        .into(),
        body: vec![],
    }
}

#[tokio::test]
async fn test_uri_normalization_rejects_long_from() {
    let config = Arc::new(ProxyConfig {
        uri_reject_malformed: true,
        uri_max_length: 20,
        ..Default::default()
    });
    let module = AclModule::new(config);

    let request = create_request_with_from_uri("sip:verylongusername@example.com");
    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(result, ProxyAction::Abort),
        "expected Abort for long URI, got {:?}",
        result
    );
}

#[tokio::test]
async fn test_uri_normalization_allows_short_from() {
    let config = Arc::new(ProxyConfig {
        uri_reject_malformed: true,
        uri_max_length: 50,
        ..Default::default()
    });
    let module = AclModule::new(config);

    let request = create_request_with_from_uri("sip:alice@example.com");
    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(result, ProxyAction::Continue),
        "expected Continue for short URI, got {:?}",
        result
    );
}

#[tokio::test]
async fn test_uri_normalization_disabled_allows_long() {
    let config = Arc::new(ProxyConfig {
        uri_reject_malformed: false,
        uri_max_length: 20,
        ..Default::default()
    });
    let module = AclModule::new(config);

    let request = create_request_with_from_uri("sip:verylongusername@example.com");
    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(result, ProxyAction::Continue),
        "expected Continue when disabled, got {:?}",
        result
    );
}
