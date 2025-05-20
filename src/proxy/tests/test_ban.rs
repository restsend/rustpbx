use crate::config::ProxyConfig;
use crate::proxy::ban::BanModule;
use crate::proxy::locator::MemoryLocator;
use crate::proxy::server::SipServerInner;
use crate::proxy::user::MemoryUserBackend;
use crate::proxy::{ProxyAction, ProxyModule};
use rsip::prelude::*;
use rsip::Header;
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::random_text;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::TransportLayer;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// Helper function to create a test request
fn create_test_request(
    method: rsip::Method,
    username: &str,
    realm: &str,
    source_ip: Option<&str>,
) -> rsip::Request {
    let host_with_port = rsip::HostWithPort {
        host: realm.parse().unwrap(),
        port: Some(5060.into()),
    };

    let uri = rsip::Uri {
        scheme: Some(rsip::Scheme::Sip),
        auth: Some(rsip::Auth {
            user: username.to_string(),
            password: None,
        }),
        host_with_port: host_with_port.clone(),
        params: vec![],
        headers: vec![],
    };

    let from = rsip::typed::From {
        display_name: None,
        uri: uri.clone(),
        params: vec![rsip::Param::Tag(rsip::param::Tag::new(random_text(8)))],
    };

    let to = rsip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    // Use a real IP in the Via header
    let via_ip = source_ip.unwrap_or("127.0.0.1");
    let via = rsip::headers::Via::new(format!(
        "SIP/2.0/UDP {}:5060;branch=z9hG4bK{}",
        via_ip,
        random_text(8)
    ));

    let call_id = rsip::headers::CallId::new(random_text(16));
    let cseq = rsip::headers::typed::CSeq {
        seq: 1u32.into(),
        method: method.clone(),
    };

    rsip::Request {
        method,
        uri,
        version: rsip::Version::V2,
        headers: vec![
            from.into(),
            to.into(),
            via.into(),
            call_id.into(),
            cseq.into(),
        ]
        .into(),
        body: vec![],
    }
}

#[tokio::test]
async fn test_ban_module_allow_normal_request() {
    // Create test server
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(ProxyConfig::default());

    let _server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Create a normal request
    let request = create_test_request(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("127.0.0.1"),
    );

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let transport_layer = TransportLayer::new(CancellationToken::new());
    let endpoint_inner = EndpointInner::new(
        "RustPBX Test".to_string(),
        transport_layer,
        CancellationToken::new(),
        Some(Duration::from_millis(20)),
        vec![rsip::Method::Invite, rsip::Method::Register],
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let mut tx = Transaction::new_server(key, request, endpoint_inner, None);

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
    // Create test server
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let mut config = ProxyConfig::default();

    // Add a denied IP to the config
    config.denies = Some(vec!["192.168.1.100".to_string()]);
    let config = Arc::new(config);

    let _server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Create a request from denied IP
    let request = create_test_request(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("192.168.1.100"),
    );

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let transport_layer = TransportLayer::new(CancellationToken::new());
    let endpoint_inner = EndpointInner::new(
        "RustPBX Test".to_string(),
        transport_layer,
        CancellationToken::new(),
        Some(Duration::from_millis(20)),
        vec![rsip::Method::Invite, rsip::Method::Register],
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let mut tx = Transaction::new_server(key, request, endpoint_inner, None);

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
    // Create test server
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let mut config = ProxyConfig::default();

    // Add an allowed IP to the config
    config.allows = Some(vec!["192.168.1.100".to_string()]);
    let config = Arc::new(config);

    let _server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Create a request from allowed IP
    let request = create_test_request(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("192.168.1.100"),
    );

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let transport_layer = TransportLayer::new(CancellationToken::new());
    let endpoint_inner = EndpointInner::new(
        "RustPBX Test".to_string(),
        transport_layer,
        CancellationToken::new(),
        Some(Duration::from_millis(20)),
        vec![rsip::Method::Invite, rsip::Method::Register],
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let mut tx = Transaction::new_server(key, request, endpoint_inner, None);

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
    // Create test server
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let mut config = ProxyConfig::default();

    // Add an allowed IP to the config (but we'll test with a different IP)
    config.allows = Some(vec!["192.168.1.100".to_string()]);
    let config = Arc::new(config);

    let _server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Create a request from a different IP
    let request = create_test_request(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("192.168.1.101"),
    );

    // Create the ban module
    let module = BanModule::new(config);

    // Create a transaction
    let transport_layer = TransportLayer::new(CancellationToken::new());
    let endpoint_inner = EndpointInner::new(
        "RustPBX Test".to_string(),
        transport_layer,
        CancellationToken::new(),
        Some(Duration::from_millis(20)),
        vec![rsip::Method::Invite, rsip::Method::Register],
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let mut tx = Transaction::new_server(key, request, endpoint_inner, None);

    // Test ban module
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since IP is not in the allowed list
    assert!(matches!(result, ProxyAction::Abort));
}
