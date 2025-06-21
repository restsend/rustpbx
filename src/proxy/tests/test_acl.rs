use super::common::{create_acl_request, create_transaction};
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
    let request = create_acl_request(rsip::Method::Invite, "alice", "127.0.0.1");

    // Create the_acl module
    let module = AclModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test_acl module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since it's a normal request
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_acl_module_block_denied_ip() {
    // Add a denied IP to the config
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec!["deny 192.168.1.100".to_string()]);
    let config = Arc::new(config);

    // Create a request from denied IP
    let request = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.100");

    // Create the_acl module
    let module = AclModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test_acl module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since IP is denied
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_acl_module_allow_specific_ip() {
    // Add an allowed IP to the config
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec!["allow 192.168.1.100".to_string()]);
    let config = Arc::new(config);

    // Create a request from allowed IP
    let request = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.100");

    // Create the_acl module
    let module = AclModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test_acl module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since IP is explicitly allowed
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_acl_module_block_not_allowed_ip() {
    // Add an allowed IP to the config (but we'll test with a different IP)
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec!["allow 192.168.1.100".to_string()]);
    let config = Arc::new(config);

    // Create a request from a different IP (not allowed)
    let request = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.101");

    // Create the_acl module
    let module = AclModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test_acl module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since IP is not explicitly allowed
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_acl_cidr_rules() {
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec![
        "deny 192.168.1.100".to_string(),
        "allow 192.168.1.0/24".to_string(),
        "deny all".to_string(),
    ]);
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Test allowed IP in CIDR range
    let request1 = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.1");
    let (mut tx1, _) = create_transaction(request1).await;
    assert!(matches!(
        module
            .on_transaction_begin(CancellationToken::new(), &mut tx1)
            .await
            .unwrap(),
        ProxyAction::Continue
    ));

    // Test denied IP in CIDR range
    let request2 = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.100");
    let (mut tx2, _) = create_transaction(request2).await;
    assert!(matches!(
        module
            .on_transaction_begin(CancellationToken::new(), &mut tx2)
            .await
            .unwrap(),
        ProxyAction::Abort
    ));
}

#[tokio::test]
async fn test_acl_invalid_rules() {
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec!["invalid_rule".to_string(), "allow all".to_string()]);
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Should use default rules when invalid rules are present
    let request = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.1");
    let (mut tx, _) = create_transaction(request).await;
    assert!(matches!(
        module
            .on_transaction_begin(CancellationToken::new(), &mut tx)
            .await
            .unwrap(),
        ProxyAction::Continue
    ));
}

#[tokio::test]
async fn test_acl_ipv6() {
    // Test IPv6 support directly using the AclModule unit tests
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec![
        "allow 2001:db8::/32".to_string(),
        "deny all".to_string(),
    ]);
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Test allowed IPv6 directly
    let ipv6_allowed: std::net::IpAddr = "2001:db8::1".parse().unwrap();
    assert!(module.is_allowed(&ipv6_allowed));

    // Test denied IPv6 directly
    let ipv6_denied: std::net::IpAddr = "2001:db9::1".parse().unwrap();
    assert!(!module.is_allowed(&ipv6_denied));
}

#[tokio::test]
async fn test_acl_rule_order() {
    let mut config = ProxyConfig::default();
    config.acl_rules = Some(vec![
        "deny all".to_string(),
        "allow 192.168.1.100".to_string(),
    ]);
    let config = Arc::new(config);

    let module = AclModule::new(config);

    // Test that deny all takes precedence
    let request = create_acl_request(rsip::Method::Invite, "alice", "192.168.1.100");
    let (mut tx, _) = create_transaction(request).await;
    assert!(matches!(
        module
            .on_transaction_begin(CancellationToken::new(), &mut tx)
            .await
            .unwrap(),
        ProxyAction::Abort
    ));
}
