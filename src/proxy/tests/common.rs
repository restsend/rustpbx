use crate::call::SipUser;
use crate::config::{ProxyConfig, RtpConfig};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::server::SipServerInner;
use crate::proxy::user::MemoryUserBackend;
use crate::proxy::{
    data::ProxyDataContext,
    locator::{Locator, MemoryLocator},
};
use rsip::Header;
use rsip::services::DigestGenerator;
use rsip::{HostWithPort, prelude::*};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::random_text;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipAddr;
use rsipstack::transport::channel::ChannelConnection;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Creates a test SIP server with a memory user backend and locator
pub async fn create_test_server() -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    create_test_server_with_config(ProxyConfig::default()).await
}

/// Creates a test SIP server with custom config
pub async fn create_test_server_with_config(
    mut config: ProxyConfig,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    // Add rustpbx.com to the allowed realms for testing
    if config.realms.is_none() {
        config.realms = Some(vec![]);
    }
    config
        .realms
        .as_mut()
        .unwrap()
        .push("rustpbx.com".to_string());

    let user_backend = Box::new(MemoryUserBackend::new(None));
    let locator = Arc::new(Box::new(MemoryLocator::new()) as Box<dyn Locator>);
    let config = Arc::new(config);

    let endpoint = rsipstack::EndpointBuilder::new().build();
    // Add a mock transport to the endpoint so it can send out-of-dialog requests in tests
    let (tx_chan, _rx_chan) = tokio::sync::mpsc::unbounded_channel();
    let mock_addr = SipAddr {
        r#type: Some(rsip::Transport::Udp),
        addr: rsip::HostWithPort {
            host: "127.0.0.1".parse().unwrap(),
            port: Some(5060.into()),
        },
    };
    let connection = ChannelConnection::create_connection(_rx_chan, tx_chan, mock_addr, None)
        .await
        .expect("failed to create channel connection");
    endpoint
        .inner
        .transport_layer
        .add_transport(connection.into());

    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    // Create server inner directly
    let data_context = Arc::new(
        ProxyDataContext::new(config.clone(), None)
            .await
            .expect("failed to init proxy data context for tests"),
    );

    let (locator_events_tx, _) = tokio::sync::broadcast::channel(100);

    let server_inner = Arc::new(SipServerInner {
        rtp_config: RtpConfig::default(),
        proxy_config: config.clone(),
        cancel_token: CancellationToken::new(),
        data_context,
        database: None,
        user_backend: user_backend,
        auth_backend: Vec::new(),
        call_router: None,
        locator: locator,
        callrecord_sender: None,
        endpoint,
        dialog_layer,
        dialplan_inspectors: Vec::new(),
        create_route_invite: None,
        ignore_out_of_dialog_request: true,
        locator_events: Some(locator_events_tx),
        sip_flow: None,
        active_call_registry: Arc::new(ActiveProxyCallRegistry::new()),
        frequency_limiter: None,
        call_record_hooks: Arc::new(Vec::new()),
        runnings_tx: Arc::new(AtomicUsize::new(0)),
        storage: None,
        presence_manager: Arc::new(crate::proxy::presence::PresenceManager::new(None)),
    });

    // Add test users
    let enabled_user = SipUser {
        id: 1,
        username: "alice".to_string(),
        password: Some("password".to_string()),
        enabled: true,
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
    };

    let disabled_user = SipUser {
        id: 2,
        username: "bob".to_string(),
        password: Some("password".to_string()),
        enabled: false,
        realm: Some("rustpbx.com".to_string()),
        ..Default::default()
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

/// Creates a basic SIP transaction for testing
pub async fn create_transaction(request: rsip::Request) -> (Transaction, Arc<EndpointInner>) {
    let mock_addr = SipAddr {
        r#type: Some(rsip::Transport::Udp),
        addr: HostWithPort {
            host: "127.0.0.1".parse().unwrap(),
            port: Some(5060.into()),
        },
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let connection = ChannelConnection::create_connection(rx, tx, mock_addr, None)
        .await
        .expect("failed to create channel connection");
    let transport_layer = rsipstack::transport::TransportLayer::new(CancellationToken::new());
    let sip_conn: rsipstack::transport::SipConnection = connection.into();
    transport_layer.add_transport(sip_conn.clone());

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
    let tx = Transaction::new_server(key, request, endpoint_inner.clone(), Some(sip_conn));

    (tx, endpoint_inner)
}

pub fn create_test_request(
    method: rsip::Method,
    username: &str,
    password: Option<&str>,
    realm: &str,
    expires: Option<u32>,
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
        method,
    };

    // Create contact with the same user and host
    let contact_uri = rsip::Uri {
        scheme: Some(rsip::Scheme::Sip),
        auth: Some(rsip::Auth {
            user: username.to_string(),
            password: password.map(|p| p.to_string()),
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
    if let Some(password) = password {
        let demo_nonce = "demo_nonce";
        let digest = DigestGenerator {
            username,
            password,
            algorithm: rsip::headers::auth::Algorithm::Md5,
            nonce: demo_nonce,
            method: &method,
            uri: &uri,
            realm,
            qop: None,
        };

        let auth_header = rsip::headers::Authorization::new(format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            username,
            realm,
            demo_nonce,
            uri.to_string(),
            digest.compute()
        ));
        headers.push(auth_header.into());
    }

    rsip::Request {
        method,
        uri: uri.clone(),
        version: rsip::Version::V2,
        headers: headers.into(),
        body: vec![],
    }
}
/// Creates a test REGISTER request
pub fn create_register_request(username: &str, realm: &str, expires: Option<u32>) -> rsip::Request {
    create_test_request(rsip::Method::Register, username, None, realm, expires)
}

/// Creates a test request with proper authentication
pub fn create_auth_request(
    method: rsip::Method,
    username: &str,
    realm: &str,
    password: &str,
) -> rsip::Request {
    create_test_request(method, username, Some(password), realm, None)
}

/// Creates a test request with specific source IP (for acl module tests)
pub fn create_acl_request(method: rsip::Method, username: &str, realm: &str) -> rsip::Request {
    create_test_request(method, username, None, realm, None)
}

/// Creates a test request with proper proxy authentication
pub fn create_proxy_auth_request_with_nonce(
    method: rsip::Method,
    username: &str,
    realm: &str,
    password: Option<&str>,
    nonce: &str,
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
        method,
    };

    // Create contact with the same user and host
    let contact_uri = rsip::Uri {
        scheme: Some(rsip::Scheme::Sip),
        auth: Some(rsip::Auth {
            user: username.to_string(),
            password: password.map(|p| p.to_string()),
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

    // Add Proxy-Authorization header if password is provided
    if let Some(password) = password {
        let digest = DigestGenerator {
            username,
            password,
            algorithm: rsip::headers::auth::Algorithm::Md5,
            nonce,
            method: &method,
            uri: &uri,
            realm,
            qop: None,
        };

        let proxy_auth_header = rsip::headers::ProxyAuthorization::new(format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            username,
            realm,
            nonce,
            uri.to_string(),
            digest.compute()
        ));
        headers.push(proxy_auth_header.into());
    }

    rsip::Request {
        method,
        uri: uri.clone(),
        version: rsip::Version::V2,
        headers: headers.into(),
        body: vec![],
    }
}

/// Extracts nonce from Proxy-Authenticate header
pub fn extract_nonce_from_proxy_authenticate(response: &rsip::Response) -> Option<String> {
    for header in response.headers().iter() {
        if let Header::ProxyAuthenticate(proxy_auth) = header {
            let auth_str = proxy_auth.value();
            // Parse the Digest authentication string to extract nonce
            for part in auth_str.split(',') {
                let part = part.trim();
                if part.starts_with("nonce=") {
                    let nonce = part.strip_prefix("nonce=").unwrap().trim_matches('"');
                    return Some(nonce.to_string());
                }
            }
        }
    }
    None
}
