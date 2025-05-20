use crate::config::ProxyConfig;
use crate::proxy::locator::MemoryLocator;
use crate::proxy::server::SipServerInner;
use crate::proxy::user::{MemoryUserBackend, SipUser};
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

/// Creates a test SIP server with a memory user backend and locator
pub async fn create_test_server() -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
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

    let disabled_user = SipUser {
        id: 2,
        username: "bob".to_string(),
        password: Some("password".to_string()),
        enabled: false,
        realm: Some("example.com".to_string()),
    };

    server_inner
        .user_backend
        .create_user(enabled_user)
        .await
        .unwrap();
    server_inner
        .user_backend
        .create_user(disabled_user)
        .await
        .unwrap();

    (server_inner, config)
}

/// Creates a test SIP server with custom config
pub async fn create_test_server_with_config(
    config: ProxyConfig,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    let user_backend = Box::new(MemoryUserBackend::new());
    let locator = Box::new(MemoryLocator::new());
    let config = Arc::new(config);

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

    (server_inner, config)
}

/// Creates a basic SIP transaction for testing
pub fn create_transaction(request: rsip::Request) -> (Transaction, Arc<EndpointInner>) {
    let transport_layer = TransportLayer::new(CancellationToken::new());
    let endpoint_inner = EndpointInner::new(
        "RustPBX Test".to_string(),
        transport_layer,
        CancellationToken::new(),
        Some(Duration::from_millis(20)),
        vec![rsip::Method::Invite, rsip::Method::Register],
    );

    let key = TransactionKey::from_request(&request, TransactionRole::Server).unwrap();
    let tx = Transaction::new_server(key, request, endpoint_inner.clone(), None);

    (tx, endpoint_inner)
}

/// Creates a test REGISTER request
pub fn create_register_request(username: &str, realm: &str, expires: Option<u32>) -> rsip::Request {
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
        method: rsip::Method::Register,
    };

    // Create contact with the same user and host
    let contact_uri = rsip::Uri {
        scheme: Some(rsip::Scheme::Sip),
        auth: Some(rsip::Auth {
            user: username.to_string(),
            password: None,
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

    // Add expires header if provided
    if let Some(exp) = expires {
        headers.push(Header::Expires(exp.into()));
    }

    rsip::Request {
        method: rsip::Method::Register,
        uri: uri.clone(),
        version: rsip::Version::V2,
        headers: headers.into(),
        body: vec![],
    }
}

/// Creates a test request for any method
pub fn create_test_request(method: rsip::Method, username: &str, realm: &str) -> rsip::Request {
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

    rsip::Request {
        method,
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
    }
}

/// Creates a test request with proper authentication
pub fn create_auth_request(
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

/// Creates a test request with specific source IP (for ban module tests)
pub fn create_ban_request(
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
