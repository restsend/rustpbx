use crate::config::ProxyConfig;
use crate::proxy::auth::AuthModule;
use crate::proxy::locator::MemoryLocator;
use crate::proxy::server::SipServerInner;
use crate::proxy::user::{MemoryUserBackend, SipUser};
use crate::proxy::{ProxyAction, ProxyModule};
use rsip::prelude::*;
use rsip::services::DigestGenerator;
use rsip::Header;
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::random_text;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::TransportLayer;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// Helper function to create a test request with proper authentication
fn create_test_request(
    method: rsip::Method,
    username: &str,
    realm: &str,
    password: Option<&str>,
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

    let via = rsip::headers::Via::new(format!(
        "SIP/2.0/UDP {}:5060;branch=z9hG4bK{}",
        realm,
        random_text(8)
    ));

    let call_id = rsip::headers::CallId::new(random_text(16));
    let cseq = rsip::headers::typed::CSeq {
        seq: 1u32.into(),
        method: method.clone(),
    };

    let mut request = rsip::Request {
        method: method.clone(),
        uri: uri.clone(),
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
    };

    // Add Authorization header if password is provided
    if let Some(password) = password {
        // Use a fixed nonce for reproducibility in tests
        let nonce = "0123456789abcdef";

        // Create digest exactly as verify_credentials function expects
        let response = DigestGenerator {
            username,
            password,
            algorithm: rsip::headers::auth::Algorithm::Md5,
            nonce,
            method: &method,
            qop: None,
            uri: &uri,
            realm,
        }
        .compute();

        // Format the Authorization header exactly as expected by verification function
        let uri_str = format!("sip:{}@{}", username, realm);
        request.headers_mut().push(Header::Authorization(
            rsip::headers::Authorization::new(
                format!(
                    "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
                    username, realm, nonce, uri_str, response
                )
            )
        ));
    }

    request
}

#[tokio::test]
async fn test_auth_module_invite_success() {
    // Create test server with user backend
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(ProxyConfig::default());

    // Create server inner directly
    let server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Add test users
    let enabled_user = SipUser {
        id: 1,
        username: "alice".to_string(),
        password: Some("password".to_string()),
        enabled: true,
        realm: Some("example.com".to_string()),
    };

    server_inner
        .user_backend
        .create_user(enabled_user)
        .await
        .unwrap();

    // Create INVITE request for valid user with proper authentication
    let request = create_test_request(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("password"),
    );

    // Create the auth module
    let module = AuthModule::new(server_inner, config);

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

    // Test authentication
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since alice is enabled and properly authenticated
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_auth_module_register_success() {
    // Create test server with user backend
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(ProxyConfig::default());

    // Create server inner directly
    let server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Add test users
    let enabled_user = SipUser {
        id: 1,
        username: "alice".to_string(),
        password: Some("password".to_string()),
        enabled: true,
        realm: Some("example.com".to_string()),
    };

    server_inner
        .user_backend
        .create_user(enabled_user)
        .await
        .unwrap();

    // Create REGISTER request for valid user with proper authentication
    let request = create_test_request(
        rsip::Method::Register,
        "alice",
        "example.com",
        Some("password"),
    );

    // Create the auth module
    let module = AuthModule::new(server_inner, config);

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

    // Test authentication
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since alice is enabled and properly authenticated
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_auth_module_disabled_user() {
    // Create test server with user backend
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(ProxyConfig::default());

    // Create server inner directly
    let server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Add test users
    let disabled_user = SipUser {
        id: 2,
        username: "bob".to_string(),
        password: Some("password".to_string()),
        enabled: false,
        realm: Some("example.com".to_string()),
    };

    server_inner
        .user_backend
        .create_user(disabled_user)
        .await
        .unwrap();

    // Create INVITE request for disabled user
    let request = create_test_request(rsip::Method::Invite, "bob", "example.com", Some("password"));

    // Create the auth module
    let module = AuthModule::new(server_inner, config);

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

    // Test authentication
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since bob is disabled
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_auth_module_unknown_user() {
    // Create test server with user backend
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(ProxyConfig::default());

    // Create server inner directly
    let server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Create INVITE request for unknown user
    let request = create_test_request(rsip::Method::Invite, "unknown", "example.com", None);

    // Create the auth module
    let module = AuthModule::new(server_inner, config);

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

    // Test authentication
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should abort since the user doesn't exist
    assert!(matches!(result, ProxyAction::Abort));
}

#[tokio::test]
async fn test_auth_module_bypass_other_methods() {
    // Create test server with user backend
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(ProxyConfig::default());

    // Create server inner directly
    let server_inner = Arc::new(SipServerInner {
        config: config.clone(),
        cancel_token: CancellationToken::new(),
        user_backend: Arc::new(user_backend),
        locator: Arc::new(locator),
    });

    // Create OPTIONS request for unknown user (should bypass auth)
    let request = create_test_request(rsip::Method::Options, "unknown", "example.com", None);

    // Create the auth module
    let module = AuthModule::new(server_inner, config);

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

    // Test authentication
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since OPTIONS doesn't require auth
    assert!(matches!(result, ProxyAction::Continue));
}
