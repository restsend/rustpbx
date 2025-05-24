use super::common::{create_ban_request, create_transaction};
use crate::config::ProxyConfig;
use crate::proxy::ban::BanModule;
use crate::proxy::{ProxyAction, ProxyModule};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_ban_module_allow_normal_request() {
    // Create config
    let config = Arc::new(ProxyConfig::default());

    // Create a normal request
    let request = create_ban_request(rsip::Method::Invite, "alice", "127.0.0.1");

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

    // Test ban module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since it's a normal request
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_ban_module_block_denied_ip() {
    // Add a denied IP to the config
    let mut config = ProxyConfig::default();
    config.denies = Some(vec!["192.168.1.100".to_string()]);
    let config = Arc::new(config);

    // Create a request from denied IP
    let request = create_ban_request(rsip::Method::Invite, "alice", "192.168.1.100");

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

    // Test ban module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since IP is denied
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_ban_module_allow_specific_ip() {
    // Add an allowed IP to the config
    let mut config = ProxyConfig::default();
    config.allows = Some(vec!["192.168.1.100".to_string()]);
    let config = Arc::new(config);

    // Create a request from allowed IP
    let request = create_ban_request(rsip::Method::Invite, "alice", "192.168.1.100");

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

    // Test ban module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since IP is explicitly allowed
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_ban_module_block_not_allowed_ip() {
    // Add an allowed IP to the config (but we'll test with a different IP)
    let mut config = ProxyConfig::default();
    config.allows = Some(vec!["192.168.1.100".to_string()]);
    let config = Arc::new(config);

    // Create a request from a different IP (not allowed)
    let request = create_ban_request(rsip::Method::Invite, "alice", "192.168.1.101");

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

    // Test ban module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since IP is not explicitly allowed
    assert!(matches!(result, ProxyAction::Abort));
}
