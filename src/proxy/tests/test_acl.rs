use super::common::{create_acl_request, create_transaction};
use crate::call::TransactionCookie;
use crate::config::ProxyConfig;
use crate::proxy::acl::AclModule;
use crate::proxy::{ProxyAction, ProxyModule};
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
async fn test_acl_module_block_denied_ip() {
    // Add a denied IP to the config
    let config = ProxyConfig {
        acl_rules: Some(vec!["deny 192.168.1.100".to_string()]),
        ..Default::default()
    };
    let config = Arc::new(config);

    // Create a request from denied IP
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.100");

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

    // Should abort since IP is denied
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_acl_module_allow_specific_ip() {
    // Add an allowed IP to the config
    let config = ProxyConfig {
        acl_rules: Some(vec!["allow 192.168.1.100".to_string()]),
        ..Default::default()
    };
    let config = Arc::new(config);

    // Create a request from allowed IP
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.100");

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

    // Should continue since IP is explicitly allowed
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_acl_module_block_not_allowed_ip() {
    // Add an allowed IP to the config (but we'll test with a different IP)
    let config = ProxyConfig {
        acl_rules: Some(vec!["allow 192.168.1.100".to_string()]),
        ..Default::default()
    };
    let config = Arc::new(config);

    // Create a request from a different IP (not allowed)
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.101");

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

    // Should abort since IP is not explicitly allowed
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_acl_cidr_rules() {
    let config = ProxyConfig {
        acl_rules: Some(vec![
            "deny 192.168.1.100".to_string(),
            "allow 192.168.1.0/24".to_string(),
            "deny all".to_string(),
        ]),
        ..Default::default()
    };
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Test allowed IP in CIDR range
    let request1 = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.1");
    let (mut tx1, _) = create_transaction(request1).await;
    assert!(matches!(
        module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx1,
                TransactionCookie::default()
            )
            .await
            .unwrap(),
        ProxyAction::Continue
    ));

    // Test denied IP in CIDR range
    let request2 = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.100");
    let (mut tx2, _) = create_transaction(request2).await;
    assert!(matches!(
        module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx2,
                TransactionCookie::default()
            )
            .await
            .unwrap(),
        ProxyAction::Abort
    ));
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
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.1");
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

#[tokio::test]
async fn test_acl_rule_order() {
    let config = ProxyConfig {
        acl_rules: Some(vec![
            "deny all".to_string(),
            "allow 192.168.1.100".to_string(),
        ]),
        ..Default::default()
    };
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Test that deny all takes precedence
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "192.168.1.100");
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
        ProxyAction::Abort
    ));
}

// ── DoS protection integration tests ─────────────────────────────

#[tokio::test]
async fn test_dos_blocks_excessive_requests() {
    let config = Arc::new(ProxyConfig {
        dos_enabled: true,
        dos_max_cps_per_ip: 2,
        dos_scan_probe_threshold: 100,
        ..Default::default()
    });
    let module = AclModule::new(config);

    // All from the same IP — first 2 should pass, 3rd blocked
    for _ in 0..2 {
        let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.1");
        let (mut tx, _) = create_transaction(request).await;
        let result = module
            .on_transaction_begin(
                CancellationToken::new(),
                &mut tx,
                TransactionCookie::default(),
            )
            .await
            .unwrap();
        assert!(matches!(result, ProxyAction::Continue), "expected Continue, got {:?}", result);
    }

    // 3rd request from same IP → blocked
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.1");
    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, ProxyAction::Abort), "expected Abort, got {:?}", result);
}

#[tokio::test]
async fn test_dos_different_ip_not_affected() {
    let config = Arc::new(ProxyConfig {
        dos_enabled: true,
        dos_max_cps_per_ip: 1,
        dos_scan_probe_threshold: 100,
        ..Default::default()
    });
    let module = AclModule::new(config);

    // IP1: 2 requests → first passes, second blocked
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.1");
    let (mut tx, _) = create_transaction(request).await;
    assert!(matches!(
        module.on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default()).await.unwrap(),
        ProxyAction::Continue
    ));

    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.1");
    let (mut tx, _) = create_transaction(request).await;
    assert!(matches!(
        module.on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default()).await.unwrap(),
        ProxyAction::Abort
    ));

    // IP2: still allowed
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.2");
    let (mut tx, _) = create_transaction(request).await;
    assert!(matches!(
        module.on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default()).await.unwrap(),
        ProxyAction::Continue
    ));
}

#[tokio::test]
async fn test_dos_release_on_transaction_end() {
    let config = Arc::new(ProxyConfig {
        dos_enabled: true,
        dos_max_cps_per_ip: 100,
        dos_max_concurrent_per_ip: 1,
        dos_scan_probe_threshold: 100,
        ..Default::default()
    });
    let module = AclModule::new(config);

    // First request passes
    let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.1");
    let (mut tx1, _) = create_transaction(request).await;
    assert!(matches!(
        module.on_transaction_begin(CancellationToken::new(), &mut tx1, TransactionCookie::default()).await.unwrap(),
        ProxyAction::Continue
    ));

    // Second concurrent request blocked
    let request = create_acl_request(rsipstack::sip::Method::Invite, "bob", "10.0.0.1");
    let (mut tx2, _) = create_transaction(request).await;
    assert!(matches!(
        module.on_transaction_begin(CancellationToken::new(), &mut tx2, TransactionCookie::default()).await.unwrap(),
        ProxyAction::Abort
    ));

    // Release first transaction
    module.on_transaction_end(&mut tx1).await.unwrap();

    // Now second should pass
    let request = create_acl_request(rsipstack::sip::Method::Invite, "bob", "10.0.0.1");
    let (mut tx3, _) = create_transaction(request).await;
    assert!(matches!(
        module.on_transaction_begin(CancellationToken::new(), &mut tx3, TransactionCookie::default()).await.unwrap(),
        ProxyAction::Continue
    ));
}

#[tokio::test]
async fn test_dos_disabled_allows_all() {
    let config = Arc::new(ProxyConfig {
        dos_enabled: false,
        dos_max_cps_per_ip: 1, // Would block if enabled
        ..Default::default()
    });
    let module = AclModule::new(config);

    // Many requests from same IP should all pass when dos is disabled
    for _ in 0..10 {
        let request = create_acl_request(rsipstack::sip::Method::Invite, "alice", "10.0.0.1");
        let (mut tx, _) = create_transaction(request).await;
        assert!(matches!(
            module.on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default()).await.unwrap(),
            ProxyAction::Continue
        ));
    }
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
        params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new("abc123"))],
    };

    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    let via = rsipstack::sip::headers::Via::new(
        "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-testbranch",
    );

    let call_id = rsipstack::sip::headers::CallId::new("testcallid");
    let cseq = rsipstack::sip::headers::typed::CSeq { seq: 1u32, method: rsipstack::sip::Method::Invite };

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
        headers: vec![from.into(), to.into(), via.into(), call_id.into(), cseq.into(), contact.into()].into(),
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
    assert!(matches!(result, ProxyAction::Abort), "expected Abort for long URI, got {:?}", result);
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
    assert!(matches!(result, ProxyAction::Continue), "expected Continue for short URI, got {:?}", result);
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
    assert!(matches!(result, ProxyAction::Continue), "expected Continue when disabled, got {:?}", result);
}
