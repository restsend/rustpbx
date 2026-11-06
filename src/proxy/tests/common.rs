use crate::call::SipUser;
use crate::config::{MediaProxyMode, ProxyConfig, RtpConfig, SipContactConfig};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::server::SipServerInner;
use crate::proxy::user::MemoryUserBackend;
use crate::proxy::{
    data::ProxyDataContext,
    locator::{Locator, MemoryLocator},
};
use arc_swap::ArcSwap;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::Header;
use rsipstack::sip::services::DigestGenerator;
use rsipstack::sip::{HostWithPort, prelude::*};
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
    config: ProxyConfig,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    create_test_server_with_config_and_sipflow_backend(config, None).await
}

pub async fn create_test_server_with_config_and_sipflow_backend(
    config: ProxyConfig,
    sipflow_backend: Option<Arc<dyn crate::sipflow::SipFlowBackend>>,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    let locator = Arc::new(Box::new(MemoryLocator::new()) as Box<dyn Locator>);
    create_test_server_with_dependencies(config, sipflow_backend, locator, Vec::new(), None).await
}

/// Like [`create_test_server`] but with session hooks pre-installed (e.g.
/// `IvrExecHook` for app-exit restore lifecycle tests).
pub async fn create_test_server_with_session_hooks(
    config: ProxyConfig,
    hooks: Vec<Arc<dyn crate::proxy::proxy_call::session_hooks::CallSessionHook>>,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    let locator = Arc::new(Box::new(MemoryLocator::new()) as Box<dyn Locator>);
    create_test_server_with_dependencies(config, None, locator, hooks, None).await
}

pub async fn create_test_server_with_config_and_locator(
    config: ProxyConfig,
    locator: Arc<Box<dyn Locator>>,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    create_test_server_with_dependencies(config, None, locator, Vec::new(), None).await
}

/// Like [`create_test_server_with_config`] but with an RWI gateway installed
/// on the server, so session-side event emissions become observable through
/// the gateway's event tap.
pub async fn create_test_server_with_rwi_gateway(
    config: ProxyConfig,
    rwi_gateway: crate::rwi::RwiGatewayRef,
) -> (Arc<SipServerInner>, Arc<ProxyConfig>) {
    let locator = Arc::new(Box::new(MemoryLocator::new()) as Box<dyn Locator>);
    create_test_server_with_dependencies(config, None, locator, Vec::new(), Some(rwi_gateway)).await
}

async fn create_test_server_with_dependencies(
    mut config: ProxyConfig,
    sipflow_backend: Option<Arc<dyn crate::sipflow::SipFlowBackend>>,
    locator: Arc<Box<dyn Locator>>,
    session_hooks: Vec<Arc<dyn crate::proxy::proxy_call::session_hooks::CallSessionHook>>,
    rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
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
    let config = Arc::new(config);

    let endpoint = rsipstack::EndpointBuilder::new().build();
    // Add a mock transport to the endpoint so it can send out-of-dialog requests in tests
    let (tx_chan, _rx_chan) = tokio::sync::mpsc::unbounded_channel();
    let mock_addr = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
        addr: rsipstack::sip::HostWithPort {
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
    let locator_event_lock = Arc::new(tokio::sync::Mutex::new(()));
    locator.set_event_lock(Some(locator_event_lock.clone()));

    // Share ONE ConferenceManager between the conference_manager field and the
    // ConferenceServer so tests that cross between them observe the same state.
    let conf_mgr = Arc::new(crate::call::runtime::ConferenceManager::new());
    let conf_server = Arc::new(crate::call::runtime::ConferenceServer::new(
        conf_mgr.clone(),
    ));

    let presence_manager = Arc::new(
        crate::proxy::presence::PresenceManager::new(None)
            .with_registration_guard(locator.clone(), locator_event_lock.clone()),
    );

    let server_inner = Arc::new(SipServerInner {
        http_client: reqwest::Client::new(),
        rtp_config: ArcSwap::new(Arc::new(RtpConfig::default())),
        sip_contact_config: ArcSwap::new(Arc::new(SipContactConfig::default())),
        network_profiles: ArcSwap::new(Arc::new(Vec::new())),
        default_network_profile_id: ArcSwap::new(Arc::new("default".to_string())),
        media_proxy: ArcSwap::new(Arc::new(MediaProxyMode::default())),
        recording_policy: Arc::new(ArcSwap::new(Arc::new(None))),
        proxy_config: ArcSwap::from_pointee(config.as_ref().clone()),
        emergency_config: ArcSwap::from_pointee(config.emergency.clone()),
        rwi_webhook_config: ArcSwap::from_pointee(None),
        cancel_token: CancellationToken::new(),
        data_context,
        routing_state: Arc::new(parking_lot::RwLock::new(Arc::new(
            crate::call::RoutingState::new(),
        ))),
        database: None,
        user_backend,
        auth_backend: Vec::new(),
        registration_response_hook: None,
        call_router: None,
        locator,
        callrecord_sender: None,
        endpoint,
        // Test harness builds the server directly (no builder snapshot):
        // empty listen list keeps `is_same_realm` on its user_backend
        // fallback, which is what the proxy tests exercise.
        local_listen_addrs: Vec::new(),
        dialog_layer,
        dialplan_inspectors: Arc::new(parking_lot::RwLock::new(Vec::new())),
        create_route_invites: Vec::new(),
        ignore_out_of_dialog_request: true,
        locator_events: Some(locator_events_tx),
        locator_event_lock,
        sipflow_config: Arc::new(ArcSwap::new(Arc::new(None))),
        sip_flow: sipflow_backend
            .map(|backend| crate::callrecord::sipflow::SipFlow::new(Some(backend), Vec::new())),
        active_call_registry: Arc::new(ActiveProxyCallRegistry::new()),
        frequency_limiter: None,
        call_record_hooks: Arc::new(Vec::new()),
        runnings_tx: Arc::new(AtomicUsize::new(0)),
        storage: None,
        presence_manager,
        addon_registry: None,
        bans: None,
        rwi_gateway,
        ivr_trace: None,
        tls_listener: None,
        bound_transports: std::sync::Mutex::new(Vec::new()),
        conference_manager: conf_mgr,
        conference_server: conf_server,
        agent_registry: None,
        queue_location_enricher: None,
        quick_route_resolver: None,
        transfer_notify_subscribers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        cluster_event_hub: None,
        cluster_peer_ips: vec![],
        cluster_peers: vec![],
        cluster_self_addr: None,
        session_registry: Arc::new(crate::call::runtime::NoopSessionRegistry),
        session_registry_heartbeat: std::sync::Mutex::new(None),
        trunk_health: None,
        session_hooks: Arc::new(session_hooks),
        contact_username: "rustpbx".to_string(),
        rtc_cname: "test-cname".to_string(),
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
pub async fn create_transaction(
    request: rsipstack::sip::Request,
) -> (Transaction, Arc<EndpointInner>) {
    let mock_addr = SipAddr {
        r#type: Some(rsipstack::sip::Transport::Udp),
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
        vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Register,
        ],
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
    method: rsipstack::sip::Method,
    username: &str,
    password: Option<&str>,
    realm: &str,
    expires: Option<u32>,
) -> rsipstack::sip::Request {
    let host_with_port = rsipstack::sip::HostWithPort {
        host: realm.parse().unwrap(),
        port: Some(5060.into()),
    };

    let uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: username.to_string(),
            password: None,
        }),
        host_with_port: host_with_port.clone(),
        params: vec![],
        headers: vec![],
    };

    let from = rsipstack::sip::typed::From {
        display_name: None,
        uri: uri.clone(),
        params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
            random_text(8),
        ))],
    };

    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    let via = rsipstack::sip::headers::Via::new(format!(
        "SIP/2.0/UDP {}:5060;branch=z9hG4bK{}",
        realm,
        random_text(8)
    ));

    let call_id = rsipstack::sip::headers::CallId::new(random_text(16));
    let cseq = rsipstack::sip::headers::typed::CSeq { seq: 1u32, method: method.clone() };

    // Create contact with the same user and host
    let contact_uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: username.to_string(),
            password: password.map(|p| p.to_string()),
        }),
        host_with_port: host_with_port.clone(),
        params: vec![],
        headers: vec![],
    };

    let contact = rsipstack::sip::typed::Contact {
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
            algorithm: rsipstack::sip::headers::auth::Algorithm::Md5,
            nonce: demo_nonce,
            method: &method,
            uri: &uri,
            realm,
            qop: None,
        };

        let auth_header = rsipstack::sip::headers::Authorization::new(format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            username,
            realm,
            demo_nonce,
            uri,
            digest.compute()
        ));
        headers.push(auth_header.into());
    }

    rsipstack::sip::Request {
        method,
        uri: uri.clone(),
        version: rsipstack::sip::Version::V2,
        headers: headers.into(),
        body: vec![],
    }
}
/// Creates a test REGISTER request
pub fn create_register_request(
    username: &str,
    realm: &str,
    expires: Option<u32>,
) -> rsipstack::sip::Request {
    create_test_request(
        rsipstack::sip::Method::Register,
        username,
        None,
        realm,
        expires,
    )
}

/// Creates a test request with proper authentication
pub fn create_auth_request(
    method: rsipstack::sip::Method,
    username: &str,
    realm: &str,
    password: &str,
) -> rsipstack::sip::Request {
    create_test_request(method, username, Some(password), realm, None)
}

/// Creates a test request for ACL module tests.
pub fn create_acl_request(
    method: rsipstack::sip::Method,
    username: &str,
    realm: &str,
) -> rsipstack::sip::Request {
    create_test_request(method, username, None, realm, None)
}

/// Creates a test request with proper proxy authentication
pub fn create_proxy_auth_request_with_nonce(
    method: rsipstack::sip::Method,
    username: &str,
    realm: &str,
    password: Option<&str>,
    nonce: &str,
) -> rsipstack::sip::Request {
    let host_with_port = rsipstack::sip::HostWithPort {
        host: realm.parse().unwrap(),
        port: Some(5060.into()),
    };

    let uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: username.to_string(),
            password: None,
        }),
        host_with_port: host_with_port.clone(),
        params: vec![],
        headers: vec![],
    };

    let from = rsipstack::sip::typed::From {
        display_name: None,
        uri: uri.clone(),
        params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
            random_text(8),
        ))],
    };

    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    let via = rsipstack::sip::headers::Via::new(format!(
        "SIP/2.0/UDP {}:5060;branch=z9hG4bK{}",
        realm,
        random_text(8)
    ));

    let call_id = rsipstack::sip::headers::CallId::new(random_text(16));
    let cseq = rsipstack::sip::headers::typed::CSeq { seq: 1u32, method: method.clone() };

    // Create contact with the same user and host
    let contact_uri = rsipstack::sip::Uri {
        scheme: Some(rsipstack::sip::Scheme::Sip),
        auth: Some(rsipstack::sip::Auth {
            user: username.to_string(),
            password: password.map(|p| p.to_string()),
        }),
        host_with_port: host_with_port.clone(),
        params: vec![],
        headers: vec![],
    };

    let contact = rsipstack::sip::typed::Contact {
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
            algorithm: rsipstack::sip::headers::auth::Algorithm::Md5,
            nonce,
            method: &method,
            uri: &uri,
            realm,
            qop: None,
        };

        let proxy_auth_header = rsipstack::sip::headers::ProxyAuthorization::new(format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            username,
            realm,
            nonce,
            uri,
            digest.compute()
        ));
        headers.push(proxy_auth_header.into());
    }

    rsipstack::sip::Request {
        method,
        uri: uri.clone(),
        version: rsipstack::sip::Version::V2,
        headers: headers.into(),
        body: vec![],
    }
}

/// Extracts nonce from Proxy-Authenticate header
pub fn extract_nonce_from_proxy_authenticate(
    response: &rsipstack::sip::Response,
) -> Option<String> {
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

// Exercise serve -> process -> setup failure -> cleanup, including the actual
// caller dialog. Never pre-stamp its role or manufacture an Ended leg.
pub async fn outbound_failure_with(
    from: &str,
    auth_user: Option<&str>,
    contact: &str,
    status: u16,
    ringing: bool,
    configure: impl FnOnce(&mut crate::proxy::server::SipServerRef),
    before_response: impl std::future::Future<Output = ()>,
) -> Vec<crate::rwi::EventCacheEntry> {
    use crate::call::app::agent_registry::{AgentRegistry, MemoryRegistry};
    use crate::call::{DialDirection, DialStrategy, Dialplan, Location, TransactionCookie};
    use crate::proxy::proxy_call::{sip_session::SipSession, state::CallContext};
    use rsipstack::sip::{Header, Method, SipMessage, StatusCode};
    use rsipstack::transport::udp::UdpConnection;
    use std::time::Instant;

    let gateway = Arc::new(parking_lot::RwLock::new(crate::rwi::RwiGateway::new()));
    let mut events = gateway.read().subscribe_events();
    let (mut server, _) = create_test_server_with_rwi_gateway(Default::default(), gateway).await;
    let registry = Arc::new(MemoryRegistry::new());
    for agent in ["agent-a", "agent-b"] {
        registry
            .register(
                agent.into(),
                agent.into(),
                format!("sip:{agent}@rustpbx.com"),
                vec![],
                1,
            )
            .await
            .unwrap();
    }
    Arc::get_mut(&mut server).unwrap().agent_registry = Some(registry);
    configure(&mut server);
    let endpoint = server.endpoint.inner.clone();
    for addr in endpoint.transport_layer.get_addrs() {
        endpoint.transport_layer.del_transport(&addr);
    }
    let transport = UdpConnection::create_connection("127.0.0.1:0".parse().unwrap(), None, None)
        .await
        .unwrap();
    let pbx = transport.get_addr().addr.to_string();
    endpoint.transport_layer.add_transport(transport.into());
    let mut incoming = server.endpoint.incoming_transactions().unwrap();
    let serving_endpoint = endpoint.clone();
    let serving = tokio::spawn(async move { serving_endpoint.serve().await.unwrap() });

    let caller = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caller_addr = caller.local_addr().unwrap();
    let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    let call_id = uuid::Uuid::new_v4().to_string();
    let session_id = call_id.clone();
    let auth_user = auth_user.map(str::to_owned);
    let (records, mut received) = mpsc::channel(1);
    let processing = tokio::spawn(async move {
        let mut tx = incoming.recv().await.unwrap();
        assert_eq!(tx.original.method, Method::Invite);
        let original = tx.original.clone();
        let original_caller = original.from_header().unwrap().uri().unwrap().to_string();
        let mut dialplan = Dialplan::new(session_id.clone(), original, DialDirection::Outbound)
            .with_caller(original_caller.parse().unwrap())
            .with_targets(DialStrategy::Sequential(vec![Location {
                aor: format!("sip:customer@{target_addr}").parse().unwrap(),
                ..Default::default()
            }
            .into()]))
            .with_media(
                crate::call::MediaConfig::new()
                    .with_proxy_mode(crate::config::MediaProxyMode::None),
            );
        dialplan.max_ring_time = Some(Duration::from_millis(if status == 408 {
            500
        } else {
            5000
        }));
        let cookie = TransactionCookie::default();
        if let Some(username) = auth_user {
            // The validated auth boundary supplies this identity independently
            // of the original From, as JwtAuthBackend does in production.
            cookie.set_user(SipUser {
                username,
                ..Default::default()
            });
        }
        let context = CallContext {
            session_id,
            dialplan: Arc::new(dialplan),
            cookie,
            start_time: Instant::now(),
            original_caller,
            original_callee: format!("sip:customer@{pbx}"),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };
        SipSession::serve(
            server,
            context,
            &mut tx,
            CancellationToken::new(),
            Some(records),
        )
        .await
        .unwrap();
    });
    let pbx = endpoint.transport_layer.get_addrs()[0].addr.to_string();
    let to = format!("<sip:customer@{pbx}>");
    let request = |method: &str, to: &str| {
        format!(
            "{method} sip:customer@{pbx} SIP/2.0\r\nVia: SIP/2.0/UDP {caller_addr};branch=z9hG4bK{call_id}\r\nFrom: <sip:{from}@rustpbx.com>;tag=caller\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: 1 {method}\r\nContact: <sip:{contact}@{caller_addr}>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )
    };
    caller
        .send_to(request("INVITE", &to).as_bytes(), &pbx)
        .await
        .unwrap();
    let mut buffer = [0u8; 65536];
    let (size, source) =
        tokio::time::timeout(Duration::from_secs(3), target.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
    let SipMessage::Request(invite) =
        SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
    else {
        panic!("expected downstream INVITE")
    };
    assert_eq!(invite.method, Method::Invite);
    let mut response = endpoint.make_response(&invite, StatusCode::Ringing, None);
    response.headers.retain(|h| !matches!(h, Header::To(_)));
    response.headers.push(Header::To(
        format!("<sip:customer@{target_addr}>;tag=target").into(),
    ));
    before_response.await;
    if ringing {
        target
            .send_to(response.to_string().as_bytes(), source)
            .await
            .unwrap();
        // Observe progress at the caller before sending the final response;
        // otherwise the dialog may coalesce an immediately rejected ring.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (size, _) = caller.recv_from(&mut buffer).await.unwrap();
                if let SipMessage::Response(response) =
                    SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
                    && response.status_code == StatusCode::Ringing
                {
                    break;
                }
            }
        })
        .await
        .expect("caller must observe ringing before the final response");
    }
    if status == 487 {
        caller
            .send_to(request("CANCEL", &to).as_bytes(), &pbx)
            .await
            .unwrap();
    } else if status != 408 {
        response.status_code = StatusCode::Other(status, "Rejected".into());
        target
            .send_to(response.to_string().as_bytes(), source)
            .await
            .unwrap();
    }
    // ACK the actual final INVITE response so the real UAS can finish.
    let final_code = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (size, _) = caller.recv_from(&mut buffer).await.unwrap();
            if let SipMessage::Response(response) =
                SipMessage::try_from(std::str::from_utf8(&buffer[..size]).unwrap()).unwrap()
            {
                if response.status_code.code() >= 300
                    && response.cseq_header().unwrap().typed().unwrap().method == Method::Invite
                {
                    caller
                        .send_to(
                            request("ACK", response.to_header().unwrap().value()).as_bytes(),
                            &pbx,
                        )
                        .await
                        .unwrap();
                    break response.status_code.code();
                }
            }
        }
    })
    .await
    .expect("caller must receive a final failure response");
    assert_eq!(final_code, status);
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            let finished =
                event.event.event_type == "call_hangup" && event.event.payload["scope"] == "call";
            observed.push(event);
            if finished {
                break;
            }
        }
    })
    .await
    .expect("terminal cleanup must emit the session hangup");
    let record = tokio::time::timeout(Duration::from_secs(3), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.call_id, call_id);
    assert!(
        record.answer_time.is_none(),
        "failed setup must remain unanswered"
    );
    assert!(
        record.recorder.is_empty(),
        "unanswered calls with recording disabled create no artifacts"
    );
    assert!(
        observed
            .iter()
            .all(|entry| entry.event.payload["call_id"] == call_id)
    );
    assert!(
        observed
            .iter()
            .all(|entry| entry.event.event_type != "call_answered")
    );
    tokio::time::timeout(Duration::from_secs(5), processing)
        .await
        .unwrap()
        .unwrap();
    serving.abort();
    observed
}
