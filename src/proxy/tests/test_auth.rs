use std::{sync::Arc, time::Duration};

use super::common::{
    create_auth_request, create_proxy_auth_request_with_nonce, create_test_request,
    create_test_server, create_transaction, extract_nonce_from_proxy_authenticate,
};
use crate::call::{SipUser, TransactionCookie};
use crate::config::{ProxyConfig, RtpConfig};
use crate::proxy::auth::AuthModule;
use crate::proxy::data::ProxyDataContext;
use crate::proxy::locator::{Locator, MemoryLocator};
use crate::proxy::server::SipServerInner;
use crate::proxy::user::MemoryUserBackend;
use crate::proxy::{ProxyAction, ProxyModule};
use rsip::Header;
use rsip::prelude::{HasHeaders, HeadersExt, UntypedHeader};
use rsip::services::DigestGenerator;
use rsipstack::EndpointBuilder;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::random_text;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::TransportLayer;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_auth_module_invite_success() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;
    let module = AuthModule::new(server_inner.clone());

    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);
    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, ProxyAction::Abort));
    if tx.last_response.is_none() {
        let mut response = rsip::Response {
            version: rsip::Version::V2,
            status_code: rsip::StatusCode::Unauthorized,
            headers: tx.original.headers().clone(),
            body: vec![],
        };
        let www_auth = module.create_www_auth_challenge("example.com").unwrap();
        response.headers.push(Header::WwwAuthenticate(www_auth));
        tx.last_response = Some(response);
    }
    let response = tx.last_response.as_ref().unwrap();
    let nonce = if let Some(Header::WwwAuthenticate(h)) = response
        .headers()
        .iter()
        .find(|h| matches!(h, Header::WwwAuthenticate(_)))
    {
        // Parse nonce
        let auth_str = h.value();
        auth_str
            .split(',')
            .find_map(|part| {
                let part = part.trim();
                if part.starts_with("nonce=") {
                    Some(
                        part.trim_start_matches("nonce=")
                            .trim_matches('"')
                            .to_string(),
                    )
                } else {
                    None
                }
            })
            .unwrap()
    } else {
        panic!("No WWW-Authenticate header");
    };

    // Step 2: Request with authentication
    let request_with_auth = {
        let host_with_port = rsip::HostWithPort {
            host: "example.com".parse().unwrap(),
            port: Some(5060.into()),
        };
        let uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: "alice".to_string(),
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
            "SIP/2.0/UDP example.com:5060;branch=z9hG4bK{}",
            random_text(8)
        ));
        let call_id = rsip::headers::CallId::new(random_text(16));
        let cseq = rsip::headers::typed::CSeq {
            seq: 1u32.into(),
            method: rsip::Method::Invite,
        };
        let contact_uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: "alice".to_string(),
                password: Some("password".to_string()),
            }),
            host_with_port: host_with_port.clone(),
            params: vec![],
            headers: vec![],
        };
        let contact = rsip::typed::Contact {
            display_name: None,
            uri: contact_uri,
            params: vec![],
        };
        let mut headers = vec![
            from.into(),
            to.into(),
            via.into(),
            call_id.into(),
            cseq.into(),
            contact.into(),
        ];
        // Generate digest
        let digest = DigestGenerator {
            username: "alice",
            password: "password",
            algorithm: rsip::headers::auth::Algorithm::Md5,
            nonce: &nonce,
            method: &rsip::Method::Invite,
            uri: &uri,
            realm: "example.com",
            qop: None,
        };
        let auth_header = rsip::headers::Authorization::new(format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            nonce,
            uri.to_string(),
            digest.compute()
        ));
        headers.push(auth_header.into());
        rsip::Request {
            method: rsip::Method::Invite,
            uri: uri.clone(),
            version: rsip::Version::V2,
            headers: headers.into(),
            body: vec![],
        }
    };
    let (mut tx2, _) = create_transaction(request_with_auth).await;
    let result2 = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx2,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result2, ProxyAction::Continue));
}

#[tokio::test]
async fn test_auth_module_register_success() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;
    let module = AuthModule::new(server_inner.clone());

    let request = create_test_request(rsip::Method::Register, "alice", None, "example.com", None);
    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result, ProxyAction::Abort));
    if tx.last_response.is_none() {
        let mut response = rsip::Response {
            version: rsip::Version::V2,
            status_code: rsip::StatusCode::Unauthorized,
            headers: tx.original.headers().clone(),
            body: vec![],
        };
        let www_auth = module.create_www_auth_challenge("example.com").unwrap();
        response.headers.push(Header::WwwAuthenticate(www_auth));
        tx.last_response = Some(response);
    }
    let response = tx.last_response.as_ref().unwrap();
    let nonce = if let Some(Header::WwwAuthenticate(h)) = response
        .headers()
        .iter()
        .find(|h| matches!(h, Header::WwwAuthenticate(_)))
    {
        // Parse nonce
        let auth_str = h.value();
        auth_str
            .split(',')
            .find_map(|part| {
                let part = part.trim();
                if part.starts_with("nonce=") {
                    Some(
                        part.trim_start_matches("nonce=")
                            .trim_matches('"')
                            .to_string(),
                    )
                } else {
                    None
                }
            })
            .unwrap()
    } else {
        panic!("No WWW-Authenticate header");
    };

    // Step 2: Request with authentication
    let request_with_auth = {
        let host_with_port = rsip::HostWithPort {
            host: "example.com".parse().unwrap(),
            port: Some(5060.into()),
        };
        let uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: "alice".to_string(),
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
            "SIP/2.0/UDP example.com:5060;branch=z9hG4bK{}",
            random_text(8)
        ));
        let call_id = rsip::headers::CallId::new(random_text(16));
        let cseq = rsip::headers::typed::CSeq {
            seq: 1u32.into(),
            method: rsip::Method::Register,
        };
        let contact_uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: "alice".to_string(),
                password: Some("password".to_string()),
            }),
            host_with_port: host_with_port.clone(),
            params: vec![],
            headers: vec![],
        };
        let contact = rsip::typed::Contact {
            display_name: None,
            uri: contact_uri,
            params: vec![],
        };
        let mut headers = vec![
            from.into(),
            to.into(),
            via.into(),
            call_id.into(),
            cseq.into(),
            contact.into(),
        ];
        let digest = DigestGenerator {
            username: "alice",
            password: "password",
            algorithm: rsip::headers::auth::Algorithm::Md5,
            nonce: &nonce,
            method: &rsip::Method::Register,
            uri: &uri,
            realm: "example.com",
            qop: None,
        };
        let auth_header = rsip::headers::Authorization::new(format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            nonce,
            uri.to_string(),
            digest.compute()
        ));
        headers.push(auth_header.into());
        rsip::Request {
            method: rsip::Method::Register,
            uri: uri.clone(),
            version: rsip::Version::V2,
            headers: headers.into(),
            body: vec![],
        }
    };
    let (mut tx2, _) = create_transaction(request_with_auth).await;
    let result2 = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx2,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    assert!(matches!(result2, ProxyAction::Continue));
}

#[tokio::test]
async fn test_auth_module_disabled_user() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;

    // Create INVITE request for disabled user
    let request = create_auth_request(rsip::Method::Invite, "bob", "example.com", "password");

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test authentication
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
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
    let request = create_auth_request(rsip::Method::Invite, "unknown", "example.com", "123456");

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test authentication
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
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
    let request = create_auth_request(rsip::Method::Options, "unknown", "example.com", "123456");

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test authentication
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should continue since OPTIONS doesn't require auth
    assert!(matches!(result, ProxyAction::Continue));
}

#[tokio::test]
async fn test_guest_call_allowed_extension() {
    let mut proxy_config = ProxyConfig::default();
    if proxy_config.realms.is_none() {
        proxy_config.realms = Some(vec![]);
    }
    proxy_config
        .realms
        .as_mut()
        .unwrap()
        .push("example.com".to_string());

    let builtin_users = vec![SipUser {
        id: 2000,
        username: "2000".to_string(),
        enabled: true,
        realm: Some("example.com".to_string()),
        allow_guest_calls: true,
        ..Default::default()
    }];

    let user_backend = MemoryUserBackend::new(Some(builtin_users));
    let locator = Arc::new(Box::new(MemoryLocator::new()) as Box<dyn Locator>);
    let config = Arc::new(proxy_config);
    let endpoint = EndpointBuilder::new().build();
    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

    let data_context = Arc::new(
        ProxyDataContext::new(config.clone(), None)
            .await
            .expect("failed to init proxy data context for auth test"),
    );

    let server_inner = Arc::new(SipServerInner {
        rtp_config: RtpConfig::default(),
        proxy_config: config,
        cancel_token: CancellationToken::new(),
        data_context,
        database: None,
        user_backend: Box::new(user_backend),
        auth_backend: None,
        call_router: None,
        locator,
        callrecord_sender: None,
        endpoint,
        dialog_layer,
        dialplan_inspector: None,
        create_route_invite: None,
        proxycall_inspector: None,
        ignore_out_of_dialog_option: true,
        locator_events: None,
    });

    let module = AuthModule::new(server_inner);

    let request = {
        let realm = "example.com";
        let caller = "guestuser";
        let callee = "2000";

        let host_with_port = rsip::HostWithPort {
            host: realm.parse().unwrap(),
            port: Some(5060.into()),
        };

        let to_uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: callee.to_string(),
                password: None,
            }),
            host_with_port: host_with_port.clone(),
            params: vec![],
            headers: vec![],
        };

        let from_uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: caller.to_string(),
                password: None,
            }),
            host_with_port: host_with_port.clone(),
            params: vec![],
            headers: vec![],
        };

        let from = rsip::typed::From {
            display_name: None,
            uri: from_uri.clone(),
            params: vec![rsip::Param::Tag(rsip::param::Tag::new(random_text(8)))],
        };

        let to = rsip::typed::To {
            display_name: None,
            uri: to_uri.clone(),
            params: vec![],
        };

        let via = rsip::headers::Via::new(format!(
            "SIP/2.0/UDP {}:5060;branch=z9hG4bK{}",
            realm,
            random_text(8)
        ));

        let contact = rsip::typed::Contact {
            display_name: None,
            uri: from_uri.clone(),
            params: vec![],
        };

        let headers = vec![
            from.into(),
            to.into(),
            via.into(),
            rsip::headers::CallId::new(random_text(16)).into(),
            rsip::headers::typed::CSeq {
                seq: 1u32.into(),
                method: rsip::Method::Invite,
            }
            .into(),
            contact.into(),
        ];

        rsip::Request {
            method: rsip::Method::Invite,
            uri: to_uri,
            version: rsip::Version::V2,
            headers: headers.into(),
            body: vec![],
        }
    };

    let (mut tx, _) = create_transaction(request).await;
    let cookie = TransactionCookie::default();
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx, cookie.clone())
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Continue));
    assert!(tx.last_response.is_none());
    let stored_user = cookie.get_user().expect("caller should be stored");
    assert_eq!(stored_user.username, "guestuser");
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
        let uri_str = format!("sip:{}@{}", username, realm);
        request.headers_mut().push(Header::Authorization(
                rsip::headers::Authorization::new(
                    format!("Digest username=\"{}\", realm=\"{}\", nonce=\"random_nonce\", uri=\"{}\", response=\"invalid\"", username, realm, uri_str)
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
        None,
        None,
        None,
        None,
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let tx = Transaction::new_server(key, request, endpoint_inner, None);

    // Should return false because no credentials are provided
    let result = auth_module.authenticate_request(&tx).await.unwrap();
    assert!(result.is_none());
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
        None,
        None,
        None,
        None,
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let mut tx = Transaction::new_server(key, request, endpoint_inner, None);

    // Use the on_transaction_begin method to test the full flow
    let result = auth_module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
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
        None,
        None,
        None,
        None,
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let tx = Transaction::new_server(key, request, endpoint_inner, None);

    // Should return false because user is disabled
    let result = auth_module.authenticate_request(&tx).await.unwrap();
    println!("Authentication result: {:?}", result);
    assert!(result.is_none());
}

#[tokio::test]
async fn test_proxy_auth_invite_success() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;

    // Step 1: Send INVITE request without credentials
    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);
    let module = AuthModule::new(server_inner.clone());
    let (mut tx, _) = create_transaction(request).await;

    // This should return 407 Proxy Authentication Required
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Abort));

    // In test environment, manually create the response since reply_with failed due to no connection
    if tx.last_response.is_none() {
        // Create the response manually for testing - should be 401 with both challenges
        let mut response = rsip::Response {
            version: rsip::Version::V2,
            status_code: rsip::StatusCode::Unauthorized,
            headers: tx.original.headers().clone(),
            body: vec![],
        };

        // Add both WWW-Authenticate and Proxy-Authenticate headers
        let www_auth = module.create_www_auth_challenge("example.com").unwrap();
        let proxy_auth = module.create_proxy_auth_challenge("example.com").unwrap();
        response.headers.push(Header::WwwAuthenticate(www_auth));
        response.headers.push(Header::ProxyAuthenticate(proxy_auth));

        tx.last_response = Some(response);
    }

    // Extract nonce from the response
    let response = tx.last_response.as_ref().expect("Should have response");
    assert_eq!(response.status_code, rsip::StatusCode::Unauthorized);

    let nonce = extract_nonce_from_proxy_authenticate(response)
        .expect("Should have nonce in Proxy-Authenticate header");

    // Step 2: Send INVITE request with proper proxy authentication using the nonce
    let request_with_auth = create_proxy_auth_request_with_nonce(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("password"),
        &nonce,
    );

    let (mut tx2, _) = create_transaction(request_with_auth).await;

    // This should succeed
    let result2 = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx2,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    let auth_result = module.authenticate_request(&tx2).await.unwrap();
    assert!(
        auth_result.is_some(),
        "Authentication should succeed with correct credentials"
    );

    // Should continue since alice is enabled and properly authenticated
    assert!(matches!(result2, ProxyAction::Continue));
}

#[tokio::test]
async fn test_proxy_auth_no_credentials() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;

    // Create INVITE request for valid user with no proxy authentication
    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

    // Create the auth module
    let module = AuthModule::new(server_inner);

    // Create a transaction
    let (mut tx, _) = create_transaction(request).await;

    // Test authentication
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should abort and send 407 Proxy Authentication Required
    assert!(matches!(result, ProxyAction::Abort));

    // Check that the response has the correct status code
    if let Some(response) = &tx.last_response {
        assert_eq!(
            response.status_code,
            rsip::StatusCode::ProxyAuthenticationRequired
        );

        // Check for Proxy-Authenticate header
        let has_proxy_auth = response
            .headers()
            .iter()
            .any(|h| matches!(h, Header::ProxyAuthenticate(_)));
        assert!(
            has_proxy_auth,
            "Response should contain Proxy-Authenticate header"
        );
    }
}

#[tokio::test]
async fn test_proxy_auth_wrong_credentials() {
    // Create test server with user backend
    let (server_inner, _) = create_test_server().await;

    // Step 1: Get challenge
    let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);
    let module = AuthModule::new(server_inner.clone());
    let (mut tx, _) = create_transaction(request).await;

    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    assert!(matches!(result, ProxyAction::Abort));

    // In test environment, manually create the response since reply_with failed due to no connection
    if tx.last_response.is_none() {
        // Create the response manually for testing
        let mut response = rsip::Response {
            version: rsip::Version::V2,
            status_code: rsip::StatusCode::ProxyAuthenticationRequired,
            headers: tx.original.headers().clone(),
            body: vec![],
        };

        // Add Proxy-Authenticate header
        let proxy_auth = module.create_proxy_auth_challenge("example.com").unwrap();
        response.headers.push(Header::ProxyAuthenticate(proxy_auth));

        tx.last_response = Some(response);
    }

    // Extract nonce from the response
    let response = tx.last_response.as_ref().expect("Should have response");
    let nonce = extract_nonce_from_proxy_authenticate(response)
        .expect("Should have nonce in Proxy-Authenticate header");

    // Step 2: Send INVITE request with wrong password
    let request_with_wrong_auth = create_proxy_auth_request_with_nonce(
        rsip::Method::Invite,
        "alice",
        "example.com",
        Some("wrongpassword"),
        &nonce,
    );
    let (mut tx2, _) = create_transaction(request_with_wrong_auth).await;

    // Test authentication
    let auth_result = module.authenticate_request(&tx).await.unwrap();
    println!("Direct authentication result: {:?}", auth_result);

    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx2,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    println!("Authentication result: {:?}", result);

    // Should abort due to wrong credentials
    assert!(matches!(result, ProxyAction::Abort));
}
