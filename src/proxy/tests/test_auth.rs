use std::time::Duration;

use super::common::{create_auth_request, create_test_server, create_transaction};
use crate::proxy::auth::AuthModule;
use crate::proxy::{ProxyAction, ProxyModule};
use rsip::prelude::{HasHeaders, HeadersExt, UntypedHeader};
use rsip::Header;
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::TransportLayer;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_auth_module_invite_success() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;

    // Create INVITE request for valid user with proper authentication
    let request = create_auth_request(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("password"),
    );

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

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
    let (server_inner, _) = create_test_server().await;

    // Create REGISTER request for valid user with proper authentication
    let request = create_auth_request(
        rsip::Method::Register,
        "alice",
        "example.com",
        Some("password"),
    );

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

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
    let (server_inner, _) = create_test_server().await;

    // Create INVITE request for disabled user
    let request = create_auth_request(rsip::Method::Invite, "bob", "example.com", Some("password"));

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

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
    let (server_inner, _) = create_test_server().await;

    // Create INVITE request for unknown user
    let request = create_auth_request(rsip::Method::Invite, "unknown", "example.com", None);

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

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
    let (server_inner, _) = create_test_server().await;

    // Create OPTIONS request for unknown user (should bypass auth)
    let request = create_auth_request(rsip::Method::Options, "unknown", "example.com", None);

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request);

    // Test authentication
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should continue since OPTIONS doesn't require auth
    assert!(matches!(result, ProxyAction::Continue));
}

// Helper function to create a basic SIP request
fn create_sip_request(method: rsip::Method, username: &str, realm: &str) -> rsip::Request {
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
        params: vec![rsip::Param::Tag(rsip::param::Tag::new("fromtag"))],
    };

    let to = rsip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    let via = rsip::headers::Via::new(format!("SIP/2.0/UDP {}:5060;branch=z9hG4bKnashds7", realm));

    let call_id = rsip::headers::CallId::new("test-call-id");
    let cseq = rsip::headers::typed::CSeq {
        seq: 1u32.into(),
        method: method.clone(),
    };

    let mut request = rsip::Request {
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
    };

    // Add Authorization header for disabled user test
    if username == "bob" {
        request.headers_mut().push(Header::Authorization(
                rsip::headers::Authorization::new(
                    "Digest username=\"bob\", realm=\"example.com\", nonce=\"random_nonce\", response=\"invalid\""
                )
            ));
    }

    request
}

#[tokio::test]
async fn test_auth_no_credentials() {
    let (server, _) = create_test_server().await;
    let auth_module = AuthModule::new(server);

    // Create an INVITE request with no auth headers
    let request = create_sip_request(rsip::Method::Invite, "alice", "example.com");
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

    // Should return false because no credentials are provided
    let result = auth_module.authenticate_request(&mut tx).await.unwrap();
    assert!(!result);
}

#[tokio::test]
async fn test_auth_bypass_for_non_invite_register() {
    let (server, _) = create_test_server().await;
    let auth_module = AuthModule::new(server);

    // Create a BYE request
    let request = create_sip_request(rsip::Method::Bye, "alice", "example.com");
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

    // Use the on_transaction_begin method to test the full flow
    let result = auth_module
        .on_transaction_begin(CancellationToken::new(), &mut tx)
        .await
        .unwrap();

    // Should return ProxyAction::Continue for non-INVITE/REGISTER requests
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_auth_disabled_user() {
    let (server, _) = create_test_server().await;
    let auth_module = AuthModule::new(server);

    // Create an INVITE request for disabled user
    let request = create_sip_request(rsip::Method::Invite, "bob", "example.com");

    // Print the request details for debugging
    println!(
        "Test request: method={}, uri={}",
        request.method, request.uri
    );
    println!("From header: {}", request.from_header().unwrap());
    println!(
        "Has auth header: {}",
        request.authorization_header().is_some()
    );

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

    // Should return false because user is disabled
    let result = auth_module.authenticate_request(&mut tx).await.unwrap();
    println!("Authentication result: {}", result);
    assert!(!result);
}
