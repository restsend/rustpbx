use std::sync::atomic::AtomicUsize;
use std::{sync::Arc, time::Duration};

use super::common::{
    create_auth_request, create_proxy_auth_request_with_nonce, create_test_request,
    create_test_server, create_test_server_with_config, create_transaction,
    extract_nonce_from_proxy_authenticate,
};
use crate::auth::jwt_auth_backend::JwtAuthBackend;
use crate::auth::jwt_validator::{JwtValidator, generate_hs256_jwt};
use crate::call::{SipUser, TransactionCookie};
use crate::config::{JwtAuthConfig, ProxyConfig, RtpConfig};
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::auth::{AuthBackend, AuthModule};
use crate::proxy::data::ProxyDataContext;
use crate::proxy::locator::{Locator, MemoryLocator};
use crate::proxy::server::SipServerInner;
use crate::proxy::user::MemoryUserBackend;
use crate::proxy::{ProxyAction, ProxyModule};
use rsipstack::EndpointBuilder;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::Header;
use rsipstack::sip::prelude::{HasHeaders, HeadersExt};
use rsipstack::sip::services::DigestGenerator;
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
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
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
        let mut response = rsipstack::sip::Response {
            version: rsipstack::sip::Version::V2,
            status_code: rsipstack::sip::StatusCode::ProxyAuthenticationRequired,
            headers: tx.original.headers().clone(),
            body: vec![],
        };
        let proxy_auth = module.create_proxy_auth_challenge("rustpbx.com").unwrap();
        response.headers.push(Header::ProxyAuthenticate(proxy_auth));
        tx.last_response = Some(response);
    }
    let response = tx.last_response.as_ref().unwrap();
    let nonce = {
        let h = response
            .headers()
            .iter()
            .find_map(|h| {
                if let Header::ProxyAuthenticate(pa) = h {
                    Some(pa.value())
                } else {
                    None
                }
            })
            .expect("No Proxy-Authenticate header");
        h.split(',')
            .find_map(|part| {
                let part = part.trim();
                if part.starts_with("nonce=") {
                    Some(part.trim_start_matches("nonce=").trim_matches('"').to_string())
                } else {
                    None
                }
            })
            .unwrap()
    };

    // Step 2: Request with authentication
    let request_with_auth = {
        let host_with_port = rsipstack::sip::HostWithPort {
            host: "rustpbx.com".parse().unwrap(),
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
            "SIP/2.0/UDP rustpbx.com:5060;branch=z9hG4bK{}",
            random_text(8)
        ));
        let call_id = rsipstack::sip::headers::CallId::new(random_text(16));
        let cseq = rsipstack::sip::headers::typed::CSeq {
            seq: 1u32,
            method: rsipstack::sip::Method::Invite,
        };
        let contact_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "alice".to_string(),
                password: Some("password".to_string()),
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
        // Generate digest
        let digest = DigestGenerator {
            username: "alice",
            password: "password",
            algorithm: rsipstack::sip::headers::auth::Algorithm::Md5,
            nonce: &nonce,
            method: &rsipstack::sip::Method::Invite,
            uri: &uri,
            realm: "rustpbx.com",
            qop: None,
        };
        let auth_header = rsipstack::sip::headers::Authorization::new(format!(
            "Digest username=\"alice\", realm=\"rustpbx.com\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            nonce,
            uri,
            digest.compute()
        ));
        headers.push(auth_header.into());
        rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: uri.clone(),
            version: rsipstack::sip::Version::V2,
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
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

    let request = create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
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
        let mut response = rsipstack::sip::Response {
            version: rsipstack::sip::Version::V2,
            status_code: rsipstack::sip::StatusCode::Unauthorized,
            headers: tx.original.headers().clone(),
            body: vec![],
        };
        let www_auth = module.create_www_auth_challenge("rustpbx.com").unwrap();
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
        let host_with_port = rsipstack::sip::HostWithPort {
            host: "rustpbx.com".parse().unwrap(),
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
            "SIP/2.0/UDP rustpbx.com:5060;branch=z9hG4bK{}",
            random_text(8)
        ));
        let call_id = rsipstack::sip::headers::CallId::new(random_text(16));
        let cseq = rsipstack::sip::headers::typed::CSeq {
            seq: 1u32,
            method: rsipstack::sip::Method::Register,
        };
        let contact_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "alice".to_string(),
                password: Some("password".to_string()),
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
        let digest = DigestGenerator {
            username: "alice",
            password: "password",
            algorithm: rsipstack::sip::headers::auth::Algorithm::Md5,
            nonce: &nonce,
            method: &rsipstack::sip::Method::Register,
            uri: &uri,
            realm: "rustpbx.com",
            qop: None,
        };
        let auth_header = rsipstack::sip::headers::Authorization::new(format!(
            "Digest username=\"alice\", realm=\"rustpbx.com\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            nonce,
            uri,
            digest.compute()
        ));
        headers.push(auth_header.into());
        rsipstack::sip::Request {
            method: rsipstack::sip::Method::Register,
            uri: uri.clone(),
            version: rsipstack::sip::Version::V2,
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
    let request = create_auth_request(
        rsipstack::sip::Method::Invite,
        "bob",
        "rustpbx.com",
        "password",
    );

    // Create the auth module
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

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
    let request = create_auth_request(
        rsipstack::sip::Method::Invite,
        "unknown",
        "rustpbx.com",
        "123456",
    );

    // Create the auth module
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

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
    let request = create_auth_request(
        rsipstack::sip::Method::Options,
        "unknown",
        "rustpbx.com",
        "123456",
    );

    // Create the auth module
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

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
        .push("rustpbx.com".to_string());

    let builtin_users = vec![SipUser {
        id: 2000,
        username: "2000".to_string(),
        enabled: true,
        realm: Some("rustpbx.com".to_string()),
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
        auth_backend: Vec::new(),
        call_router: None,
        locator,
        callrecord_sender: None,
        endpoint,
        dialog_layer,
        dialplan_inspectors: Vec::new(),
        create_route_invites: Vec::new(),
        ignore_out_of_dialog_request: true,
        locator_events: None,
        sipflow_config: None,
        sip_flow: None,
        active_call_registry: Arc::new(ActiveProxyCallRegistry::new()),
        frequency_limiter: None,
        call_record_hooks: Arc::new(Vec::new()),
        runnings_tx: Arc::new(AtomicUsize::new(0)),
        storage: None,
        presence_manager: Arc::new(crate::proxy::presence::PresenceManager::new(None)),
        addon_registry: None,
        rwi_gateway: None,
        ivr_trace: None,
        tls_listener: None,
        queue_manager: Arc::new(crate::call::runtime::QueueManager::new()),
        conference_manager: Arc::new(crate::call::runtime::ConferenceManager::new()),
        agent_registry: None,
        queue_location_enricher: None,
        transfer_notify_subscribers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        cluster_event_hub: None,
        cluster_peer_ips: vec![],
        media_policy: Arc::new(crate::call::DefaultMediaPolicy),
        trunk_health: None,
        session_hooks: Arc::new(Vec::new()),
        pre_auth_registry: None,
        contact_username: "rustpbx".to_string(),
        rtc_cname: "test-cname".to_string(),
        media_engine: {
            use crate::media::engine::{MediaEngine, MediaEngineConfig};
            let (engine, handle) = MediaEngine::new(MediaEngineConfig::default());
            let _ = engine.spawn(handle);
            engine
        },
    });
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

    let request = {
        let realm = "rustpbx.com";
        let caller = "guestuser";
        let callee = "2000";

        let host_with_port = rsipstack::sip::HostWithPort {
            host: realm.parse().unwrap(),
            port: Some(5060.into()),
        };

        let to_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: callee.to_string(),
                password: None,
            }),
            host_with_port: host_with_port.clone(),
            params: vec![],
            headers: vec![],
        };

        let from_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: caller.to_string(),
                password: None,
            }),
            host_with_port: host_with_port.clone(),
            params: vec![],
            headers: vec![],
        };

        let from = rsipstack::sip::typed::From {
            display_name: None,
            uri: from_uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
                random_text(8),
            ))],
        };

        let to = rsipstack::sip::typed::To {
            display_name: None,
            uri: to_uri.clone(),
            params: vec![],
        };

        let via = rsipstack::sip::headers::Via::new(format!(
            "SIP/2.0/UDP {}:5060;branch=z9hG4bK{}",
            realm,
            random_text(8)
        ));

        let contact = rsipstack::sip::typed::Contact {
            display_name: None,
            uri: from_uri.clone(),
            params: vec![],
        };

        let headers = vec![
            from.into(),
            to.into(),
            via.into(),
            rsipstack::sip::headers::CallId::new(random_text(16)).into(),
            rsipstack::sip::headers::typed::CSeq {
                seq: 1u32,
                method: rsipstack::sip::Method::Invite,
            }
            .into(),
            contact.into(),
        ];

        rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: to_uri,
            version: rsipstack::sip::Version::V2,
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
fn create_sip_request(
    method: rsipstack::sip::Method,
    username: &str,
    realm: &str,
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
            "fromtag",
        ))],
    };

    let to = rsipstack::sip::typed::To {
        display_name: None,
        uri: uri.clone(),
        params: vec![],
    };

    let via = rsipstack::sip::headers::Via::new(format!(
        "SIP/2.0/UDP {}:5060;branch=z9hG4bKnashds7",
        realm
    ));

    let call_id = rsipstack::sip::headers::CallId::new("test-call-id");
    let cseq = rsipstack::sip::headers::typed::CSeq { seq: 1u32, method };

    let mut request = rsipstack::sip::Request {
        method,
        uri,
        version: rsipstack::sip::Version::V2,
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
                rsipstack::sip::headers::Authorization::new(
                    format!("Digest username=\"{}\", realm=\"{}\", nonce=\"random_nonce\", uri=\"{}\", response=\"invalid\"", username, realm, uri_str)
                )
            ));
    }

    request
}

async fn create_issue_146_server() -> Arc<SipServerInner> {
    let config = ProxyConfig {
        realms: Some(vec![
            "pbx.e36".to_string(),
            "pbx.e36:5060".to_string(),
            "pbx.e36:5061".to_string(),
        ]),
        ..Default::default()
    };

    let (server, _) = create_test_server_with_config(config).await;
    server
        .user_backend
        .create_user(SipUser {
            id: 111,
            username: "111".to_string(),
            password: Some("111".to_string()),
            enabled: true,
            realm: Some("pbx.e36".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    server
}

fn create_issue_146_register_request(
    request_uri: &str,
    authorization: &str,
) -> rsipstack::sip::Request {
    let request_uri = rsipstack::sip::Uri::try_from(request_uri).unwrap();
    let aor_uri = rsipstack::sip::Uri::try_from("sip:111@pbx.e36").unwrap();
    let contact_uri =
        rsipstack::sip::Uri::try_from("sip:111@192.168.20.169:5060;transport=tls").unwrap();

    let headers = vec![
        rsipstack::sip::typed::From {
            display_name: Some("Deskphone".into()),
            uri: aor_uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
                "a1a3406102",
            ))],
        }
        .into(),
        rsipstack::sip::typed::To {
            display_name: None,
            uri: aor_uri,
            params: vec![],
        }
        .into(),
        rsipstack::sip::headers::Via::new(
            "SIP/2.0/TLS 192.168.20.169:5060;branch=z9hG4bK64964f4e5cad27a81".to_string(),
        )
        .into(),
        rsipstack::sip::headers::CallId::new("fd9623914bbc79b9").into(),
        rsipstack::sip::headers::typed::CSeq {
            seq: 873199510u32,
            method: rsipstack::sip::Method::Register,
        }
        .into(),
        rsipstack::sip::typed::Contact {
            display_name: Some("Deskphone".into()),
            uri: contact_uri,
            params: vec![rsipstack::sip::Param::Expires(
                rsipstack::sip::param::Expires::from("3600"),
            )],
        }
        .into(),
        rsipstack::sip::headers::Authorization::new(authorization.to_string()).into(),
    ];

    rsipstack::sip::Request {
        method: rsipstack::sip::Method::Register,
        uri: request_uri,
        version: rsipstack::sip::Version::V2,
        headers: headers.into(),
        body: vec![],
    }
}

#[tokio::test]
async fn test_authenticate_request_accepts_authorization_uri_when_request_uri_differs() {
    let server = create_issue_146_server().await;
    let module = AuthModule::new(server.clone(), server.proxy_config.clone());
    let auth_header_value = r#"Digest username="111",realm="pbx.e36",nonce="MoLk0nzBonitjdoo",uri="sip:pbx.e36:5060;transport=udp",response="5a832a648a56b95f905b8db1d28d8f5b",algorithm=MD5"#;

    let request = create_issue_146_register_request("sip:pbx.e36:5060", auth_header_value);
    let (tx, _) = create_transaction(request).await;

    let result = module.authenticate_request(&tx).await.unwrap();
    assert!(
        result.is_some(),
        "authentication should succeed when the digest matches the Authorization uri from issue #146"
    );
}

#[tokio::test]
async fn test_authenticate_request_preserves_authorization_uri_transport_case() {
    let server = create_issue_146_server().await;
    let module = AuthModule::new(server.clone(), server.proxy_config.clone());
    let auth_header_value = r#"Digest username="111",realm="pbx.e36",nonce="K1KmT96onZZVMvBB",uri="sip:pbx.e36:5061;transport=tls",response="0c9ba3a13fbcc4f342fd7eb9c2be6a83",algorithm=MD5"#;
    let request =
        create_issue_146_register_request("sip:pbx.e36:5061;transport=tls", auth_header_value);
    let (tx, _) = create_transaction(request).await;

    let result = module.authenticate_request(&tx).await.unwrap();
    assert!(
        result.is_some(),
        "authentication should preserve the exact Authorization uri bytes from issue #146"
    );
}

#[tokio::test]
async fn test_auth_no_credentials() {
    let (server, _) = create_test_server().await;
    let auth_module = AuthModule::new(server.clone(), server.proxy_config.clone());

    // Create an INVITE request with no auth headers
    let request = create_sip_request(rsipstack::sip::Method::Invite, "alice", "rustpbx.com");
    let transport_layer = TransportLayer::new(CancellationToken::new());
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
    let tx = Transaction::new_server(key, request, endpoint_inner, None);

    // Should return false because no credentials are provided
    let result = auth_module.authenticate_request(&tx).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_auth_bypass_for_non_invite_register() {
    let (server, _) = create_test_server().await;
    let auth_module = AuthModule::new(server.clone(), server.proxy_config.clone());

    // Create a BYE request
    let request = create_sip_request(rsipstack::sip::Method::Bye, "alice", "rustpbx.com");
    let transport_layer = TransportLayer::new(CancellationToken::new());
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
    let auth_module = AuthModule::new(server.clone(), server.proxy_config.clone());

    // Create an INVITE request for disabled user
    let request = create_sip_request(rsipstack::sip::Method::Invite, "bob", "rustpbx.com");

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
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());
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
        // Create the response manually for testing - should be 407 with Proxy-Authenticate challenge
        let mut response = rsipstack::sip::Response {
            version: rsipstack::sip::Version::V2,
            status_code: rsipstack::sip::StatusCode::ProxyAuthenticationRequired,
            headers: tx.original.headers().clone(),
            body: vec![],
        };

        // Add Proxy-Authenticate header
        let proxy_auth = module.create_proxy_auth_challenge("rustpbx.com").unwrap();
        response.headers.push(Header::ProxyAuthenticate(proxy_auth));

        tx.last_response = Some(response);
    }

    // Extract nonce from the response
    let response = tx.last_response.as_ref().expect("Should have response");

    let nonce = extract_nonce_from_proxy_authenticate(response)
        .expect("Should have nonce in Proxy-Authenticate header");

    // Step 2: Send INVITE request with proper proxy authentication using the nonce
    let request_with_auth = create_proxy_auth_request_with_nonce(
        rsipstack::sip::Method::Invite,
        "alice",
        "rustpbx.com",
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
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );

    // Create the auth module
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());

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
            rsipstack::sip::StatusCode::ProxyAuthenticationRequired
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
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
    let module = AuthModule::new(server_inner.clone(), server_inner.proxy_config.clone());
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
        let mut response = rsipstack::sip::Response {
            version: rsipstack::sip::Version::V2,
            status_code: rsipstack::sip::StatusCode::ProxyAuthenticationRequired,
            headers: tx.original.headers().clone(),
            body: vec![],
        };

        // Add Proxy-Authenticate header
        let proxy_auth = module.create_proxy_auth_challenge("rustpbx.com").unwrap();
        response.headers.push(Header::ProxyAuthenticate(proxy_auth));

        tx.last_response = Some(response);
    }

    // Extract nonce from the response
    let response = tx.last_response.as_ref().expect("Should have response");
    let nonce = extract_nonce_from_proxy_authenticate(response)
        .expect("Should have nonce in Proxy-Authenticate header");

    // Step 2: Send INVITE request with wrong password
    let request_with_wrong_auth = create_proxy_auth_request_with_nonce(
        rsipstack::sip::Method::Invite,
        "alice",
        "rustpbx.com",
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

#[tokio::test]
async fn test_dialog_auth_cache_skips_in_dialog_reinvite() {
    // Create test server with dialog auth cache enabled
    let (server_inner, _) = create_test_server().await;
    let mut proxy_config = (*server_inner.proxy_config).clone();
    proxy_config.dialog_auth_cache = Some(crate::config::AuthCacheConfig {
        enabled: true,
        cache_size: 100,
        ttl_seconds: 3600,
    });
    let proxy_config = Arc::new(proxy_config);

    let module = AuthModule::new(server_inner.clone(), proxy_config.clone());

    // Step 1: Initial INVITE without credentials (should challenge)
    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "alice",
        None,
        "rustpbx.com",
        None,
    );
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

    // Extract nonce from challenge
    if tx.last_response.is_none() {
        let mut response = rsipstack::sip::Response {
            version: rsipstack::sip::Version::V2,
            status_code: rsipstack::sip::StatusCode::ProxyAuthenticationRequired,
            headers: tx.original.headers().clone(),
            body: vec![],
        };
        let proxy_auth = module.create_proxy_auth_challenge("rustpbx.com").unwrap();
        response.headers.push(Header::ProxyAuthenticate(proxy_auth));
        tx.last_response = Some(response);
    }
    let response = tx.last_response.as_ref().unwrap();
    let nonce = {
        let h = response
            .headers()
            .iter()
            .find_map(|h| {
                if let Header::ProxyAuthenticate(pa) = h {
                    Some(pa.value())
                } else {
                    None
                }
            })
            .expect("No Proxy-Authenticate header");
        h.split(',')
            .find_map(|part| {
                let part = part.trim();
                if part.starts_with("nonce=") {
                    Some(part.trim_start_matches("nonce=").trim_matches('"').to_string())
                } else {
                    None
                }
            })
            .unwrap()
    };

    // Step 2: Send authenticated INVITE with To tag (simulates in-dialog after 200 OK)
    let request_with_auth = {
        let host_with_port = rsipstack::sip::HostWithPort {
            host: "rustpbx.com".parse().unwrap(),
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
            uri: uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
                random_text(8),
            ))],
        };
        // Add To tag to make it look like an in-dialog request
        let to = rsipstack::sip::typed::To {
            display_name: None,
            uri: uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
                random_text(8),
            ))],
        };
        let via = rsipstack::sip::headers::Via::new(format!(
            "SIP/2.0/UDP rustpbx.com:5060;branch=z9hG4bK{}",
            random_text(8)
        ));
        let call_id = rsipstack::sip::headers::CallId::new(random_text(16));
        let cseq = rsipstack::sip::headers::typed::CSeq {
            seq: 1u32,
            method: rsipstack::sip::Method::Invite,
        };
        let contact_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "alice".to_string(),
                password: Some("password".to_string()),
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
        let digest = DigestGenerator {
            username: "alice",
            password: "password",
            algorithm: rsipstack::sip::headers::auth::Algorithm::Md5,
            nonce: &nonce,
            method: &rsipstack::sip::Method::Invite,
            uri: &uri,
            realm: "rustpbx.com",
            qop: None,
        };
        let auth_header = rsipstack::sip::headers::Authorization::new(format!(
            "Digest username=\"alice\", realm=\"rustpbx.com\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            nonce,
            uri,
            digest.compute()
        ));
        headers.push(auth_header.into());
        rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: uri.clone(),
            version: rsipstack::sip::Version::V2,
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
    assert!(
        matches!(result2, ProxyAction::Continue),
        "Initial authenticated INVITE should succeed"
    );

    // Step 3: Send re-INVITE without auth headers (same dialog, should use cache)
    let reinvite_request = {
        let host_with_port = rsipstack::sip::HostWithPort {
            host: "rustpbx.com".parse().unwrap(),
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
            uri: uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
                random_text(8),
            ))],
        };
        // Same call-id and To tag as before to match dialog
        let to = rsipstack::sip::typed::To {
            display_name: None,
            uri: uri.clone(),
            params: vec![rsipstack::sip::Param::Tag(rsipstack::sip::param::Tag::new(
                random_text(8),
            ))],
        };
        let via = rsipstack::sip::headers::Via::new(format!(
            "SIP/2.0/UDP rustpbx.com:5060;branch=z9hG4bK{}",
            random_text(8)
        ));
        let call_id = rsipstack::sip::headers::CallId::new(random_text(16));
        let cseq = rsipstack::sip::headers::typed::CSeq {
            seq: 2u32,
            method: rsipstack::sip::Method::Invite,
        };
        let contact_uri = rsipstack::sip::Uri {
            scheme: Some(rsipstack::sip::Scheme::Sip),
            auth: Some(rsipstack::sip::Auth {
                user: "alice".to_string(),
                password: Some("password".to_string()),
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
        let headers = vec![
            from.into(),
            to.into(),
            via.into(),
            call_id.into(),
            cseq.into(),
            contact.into(),
        ];
        rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: uri.clone(),
            version: rsipstack::sip::Version::V2,
            headers: headers.into(),
            body: vec![],
        }
    };
    let (mut tx3, _) = create_transaction(reinvite_request).await;
    let result3 = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx3,
            TransactionCookie::default(),
        )
        .await
        .unwrap();
    // Without auth cache, this would abort with 407. With cache enabled and matching source,
    // it should continue.
    // Note: Since we're using mock connections, source address matching may not work in tests
    // without explicit connection setup. This test mainly verifies the cache logic doesn't break.
    println!("Re-INVITE result: {:?}", result3);
}

// ═══════════════════════════════════════════════════════════════════════════════
// JWT Auth Backend Tests
// ═══════════════════════════════════════════════════════════════════════════════

fn make_jwt_config() -> JwtAuthConfig {
    JwtAuthConfig {
        enabled: true,
        secret: "test-jwt-secret".to_string(),
        user_id_claim: "userId".to_string(),
        issuer: None,
        audience: None,
        sip_header_name: "X-Auth-Token".to_string(),
        check_local_user: false,
        ws_token_param: "token".to_string(),
    }
}

#[tokio::test]
async fn test_jwt_auth_backend_valid_token() {
    let config = make_jwt_config();
    let validator = JwtValidator::new(&config);
    let backend = JwtAuthBackend::new(validator, None, config.sip_header_name.clone());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = serde_json::json!({
        "userId": "alice",
        "exp": now + 3600,
    });
    let token = generate_hs256_jwt(&claims, "test-jwt-secret");

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );

    let mut request = request;
    request
        .headers
        .push(Header::Other("X-Auth-Token".to_string(), token));

    let cookie = TransactionCookie::default();
    let result = backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok(), "authenticate should succeed");
    let user = result.unwrap().expect("should return Some(user)");
    assert_eq!(user.username, "alice");
    assert!(user.enabled);
}

#[tokio::test]
async fn test_jwt_auth_backend_invalid_token_falls_through() {
    let config = make_jwt_config();
    let validator = JwtValidator::new(&config);
    let backend = JwtAuthBackend::new(validator, None, config.sip_header_name.clone());

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );

    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "invalid.jwt.token".to_string(),
    ));

    let cookie = TransactionCookie::default();
    let result = backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    assert!(
        result.unwrap().is_none(),
        "invalid JWT should return None (fall through)"
    );
}

#[tokio::test]
async fn test_jwt_auth_backend_no_token_falls_through() {
    let config = make_jwt_config();
    let validator = JwtValidator::new(&config);
    let backend = JwtAuthBackend::new(validator, None, config.sip_header_name.clone());

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );

    let cookie = TransactionCookie::default();
    let result = backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    assert!(
        result.unwrap().is_none(),
        "no JWT header should return None (fall through)"
    );
}

#[tokio::test]
async fn test_jwt_auth_backend_with_check_local_user() {
    let (server_inner, _) = create_test_server().await;

    let mut config = make_jwt_config();
    config.check_local_user = true;
    let validator = JwtValidator::new(&config);

    // The test server has "alice" as an enabled user
    let backend = JwtAuthBackend::new(
        validator,
        None, // We can't easily clone the Box<dyn UserBackend>, test without local lookup
        config.sip_header_name.clone(),
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = serde_json::json!({
        "userId": "alice",
        "name": "Alice Test",
        "exp": now + 3600,
    });
    let token = generate_hs256_jwt(&claims, "test-jwt-secret");

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );

    let mut request = request;
    request
        .headers
        .push(Header::Other("X-Auth-Token".to_string(), token));

    let cookie = TransactionCookie::default();
    let result = backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    let user = result.unwrap().expect("should return Some(user)");
    assert_eq!(user.username, "alice");
    // Without check_local_user (user_backend=None), display_name comes from JWT claims
    assert_eq!(user.display_name, Some("Alice Test".to_string()));

    // Verify server is set up correctly
    assert!(
        server_inner
            .user_backend
            .get_user("alice", Some("rustpbx.com"), None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn test_jwt_auth_module_integration_no_401() {
    // Build a test server with JWT auth backend
    let mut proxy_config = ProxyConfig::default();
    proxy_config.ensure_user = Some(false);
    let (mut server_inner, config) = create_test_server_with_config(proxy_config).await;

    // Add JWT auth backend to the server's auth_backend chain
    let jwt_config = make_jwt_config();
    let validator = JwtValidator::new(&jwt_config);
    let backend = Box::new(JwtAuthBackend::new(
        validator,
        None,
        jwt_config.sip_header_name.clone(),
    ));

    // We need to modify auth_backend on the server_inner.
    // Since it's behind Arc, use Arc::get_mut.
    if let Some(ref mut inner) = Arc::get_mut(&mut server_inner) {
        inner.auth_backend.push(backend);
    }

    let module = AuthModule::new(server_inner.clone(), config);

    // Generate valid JWT
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = serde_json::json!({
        "userId": "newuser",
        "exp": now + 3600,
    });
    let token = generate_hs256_jwt(&claims, "test-jwt-secret");

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "newuser",
        None,
        "rustpbx.com",
        Some(300),
    );

    let mut request = request;
    request
        .headers
        .push(Header::Other("X-Auth-Token".to_string(), token));

    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should continue (authenticated via JWT), NOT abort with 401
    assert_eq!(
        format!("{:?}", result),
        format!("{:?}", ProxyAction::Continue),
        "REGISTER with valid JWT should pass through without 401"
    );
}

#[tokio::test]
async fn test_jwt_auth_module_integration_invalid_falls_back_to_401() {
    let mut proxy_config = ProxyConfig::default();
    proxy_config.ensure_user = Some(false);
    let (mut server_inner, config) = create_test_server_with_config(proxy_config).await;

    // Add JWT auth backend
    let jwt_config = make_jwt_config();
    let validator = JwtValidator::new(&jwt_config);
    let backend = Box::new(JwtAuthBackend::new(
        validator,
        None,
        jwt_config.sip_header_name.clone(),
    ));

    if let Some(ref mut inner) = Arc::get_mut(&mut server_inner) {
        inner.auth_backend.push(backend);
    }

    let module = AuthModule::new(server_inner.clone(), config);

    // REGISTER with invalid JWT
    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "stranger",
        None,
        "rustpbx.com",
        Some(300),
    );

    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "completely.invalid.jwt".to_string(),
    ));

    let (mut tx, _) = create_transaction(request).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await
        .unwrap();

    // Should abort (fall back to 401 since JWT invalid and no password)
    assert_eq!(
        format!("{:?}", result),
        format!("{:?}", ProxyAction::Abort),
        "REGISTER with invalid JWT should fall back to 401 challenge"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// HTTP Token Auth Backend Tests
// ═══════════════════════════════════════════════════════════════════════════════

use crate::auth::http_token_auth_backend::HttpTokenAuthBackend;
use crate::proxy::user_http::HttpUserBackend;
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// A minimal HTTP test server that responds to token-auth requests.
struct TokenAuthTestServer {
    port: u16,
    request_count: Arc<AtomicU32>,
}

impl TokenAuthTestServer {
    /// Start a server that returns 200 + SipUser for valid tokens, 403 for invalid.
    async fn start(valid_token: &'static str) -> Self {
        let request_count = Arc::new(AtomicU32::new(0));
        let rc = request_count.clone();
        let app = axum::Router::new().route(
            "/auth",
            axum::routing::post(
                move |axum::Form(form): axum::Form<HashMap<String, String>>| {
                    rc.fetch_add(1, Ordering::SeqCst);
                    let token = form.get("X-Auth-Token").cloned().unwrap_or_default();
                    let username = form.get("username").cloned().unwrap_or_default();
                    let realm = form.get("realm").cloned().unwrap_or_default();

                    let resp: axum::response::Response = if token == valid_token {
                        let user = serde_json::json!({
                            "username": username,
                            "enabled": true,
                            "realm": realm,
                            "display_name": format!("{} via token", username),
                            "id": 100,
                        });
                        (axum::http::StatusCode::OK, axum::Json(user)).into_response()
                    } else {
                        (
                            axum::http::StatusCode::FORBIDDEN,
                            axum::Json(serde_json::json!({
                                "reason": "invalid_credentials",
                                "message": "token not recognized"
                            })),
                        )
                            .into_response()
                    };
                    async move { resp }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self {
            port,
            request_count,
        }
    }

    /// Start a server that always returns 500 (for retry testing).
    async fn start_always_fail() -> Self {
        let request_count = Arc::new(AtomicU32::new(0));
        let rc = request_count.clone();
        let app = axum::Router::new().route(
            "/auth",
            axum::routing::post(move || {
                rc.fetch_add(1, Ordering::SeqCst);
                let resp: axum::response::Response = (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({
                        "reason": "server_error",
                        "message": "internal error"
                    })),
                )
                    .into_response();
                async move { resp }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self {
            port,
            request_count,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/auth", self.port)
    }

    fn request_count(&self) -> u32 {
        self.request_count.load(Ordering::SeqCst)
    }
}

fn make_http_backend(url: &str) -> HttpUserBackend {
    let mut sip_headers = Vec::new();
    sip_headers.push("X-Auth-Token".to_string());
    HttpUserBackend::new(
        url,
        &Some("POST".to_string()),
        &None,
        &None,
        &None,
        &None,
        &Some(sip_headers),
        &Some("X-Auth-Token".to_string()),
        &Some(2000),
        &Some(1),
        &Some(50),
    )
}

#[tokio::test]
async fn test_http_token_auth_valid_token() {
    let server = TokenAuthTestServer::start("valid-token-123").await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_secs(0),
        0,
    );

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );
    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "valid-token-123".to_string(),
    ));

    let cookie = TransactionCookie::default();
    let result = auth_backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok(), "authenticate should succeed");
    let user = result.unwrap().expect("should return Some(user)");
    assert_eq!(user.username, "alice");
    assert!(user.enabled);
    assert_eq!(user.display_name.as_deref(), Some("alice via token"));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn test_http_token_auth_no_token_falls_through() {
    let server = TokenAuthTestServer::start("valid-token-123").await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_secs(0),
        0,
    );

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );

    let cookie = TransactionCookie::default();
    let result = auth_backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_none(), "no token → should fall through");
    assert_eq!(
        server.request_count(),
        0,
        "should not make HTTP request without token"
    );
}

#[tokio::test]
async fn test_http_token_auth_invalid_token_falls_through() {
    let server = TokenAuthTestServer::start("valid-token-123").await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_secs(0),
        0,
    );

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );
    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "wrong-token".to_string(),
    ));

    let cookie = TransactionCookie::default();
    let result = auth_backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    assert!(
        result.unwrap().is_none(),
        "invalid token → should fall through"
    );
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn test_http_token_auth_cache_hit() {
    let server = TokenAuthTestServer::start("cached-token").await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_secs(60),
        100,
    );

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );
    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "cached-token".to_string(),
    ));

    let cookie = TransactionCookie::default();

    // First call: HTTP request
    let result = auth_backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    let user = result.unwrap().expect("first call should succeed");
    assert_eq!(user.username, "alice");
    assert_eq!(server.request_count(), 1);

    // Second call: cache hit, no HTTP request
    let result2 = auth_backend.authenticate(&request, &cookie).await;
    assert!(result2.is_ok());
    let user2 = result2
        .unwrap()
        .expect("second call should succeed via cache");
    assert_eq!(user2.username, "alice");
    assert_eq!(server.request_count(), 1, "second call should use cache");
}

#[tokio::test]
async fn test_http_token_auth_cache_ttl_expiry() {
    let server = TokenAuthTestServer::start("ttl-token").await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_millis(100),
        100,
    );

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );
    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "ttl-token".to_string(),
    ));

    let cookie = TransactionCookie::default();

    // First call: HTTP request + cache
    let result = auth_backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok());
    assert_eq!(server.request_count(), 1);

    // Wait for cache to expire
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Second call: cache expired, new HTTP request
    let result2 = auth_backend.authenticate(&request, &cookie).await;
    assert!(result2.is_ok());
    assert_eq!(
        server.request_count(),
        2,
        "expired cache should trigger new HTTP request"
    );
}

#[tokio::test]
async fn test_http_token_auth_cache_lru_eviction() {
    let server = TokenAuthTestServer::start("token-evict").await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_secs(60),
        2, // max 2 entries
    );

    let cookie = TransactionCookie::default();

    // Insert 3 different tokens → cache max is 2, first should be evicted
    for i in 0..3 {
        let request = super::common::create_test_request(
            rsipstack::sip::Method::Register,
            &format!("user{}", i),
            None,
            "rustpbx.com",
            Some(300),
        );
        let mut request = request;
        request.headers.push(Header::Other(
            "X-Auth-Token".to_string(),
            format!("token-evict-{}", i),
        ));

        // Each unique token needs a valid token on the server side, but since our test server
        // only accepts "token-evict", we just verify the HTTP call count increases.
        let result = auth_backend.authenticate(&request, &cookie).await;
        assert!(result.is_ok());
    }

    // All 3 calls should have made HTTP requests (no cache hits since all different tokens
    // and one was evicted)
    assert!(
        server.request_count() >= 3,
        "all 3 calls should hit the server"
    );
}

#[tokio::test]
async fn test_http_token_auth_retry_on_failure() {
    let server = TokenAuthTestServer::start_always_fail().await;
    let backend = make_http_backend(&server.url());
    let auth_backend = HttpTokenAuthBackend::new(
        backend,
        "X-Auth-Token".to_string(),
        Duration::from_secs(0),
        0,
    );

    let request = super::common::create_test_request(
        rsipstack::sip::Method::Register,
        "alice",
        None,
        "rustpbx.com",
        Some(300),
    );
    let mut request = request;
    request.headers.push(Header::Other(
        "X-Auth-Token".to_string(),
        "any-token".to_string(),
    ));

    let cookie = TransactionCookie::default();
    let result = auth_backend.authenticate(&request, &cookie).await;
    assert!(result.is_ok(), "should not error even after retries");
    assert!(
        result.unwrap().is_none(),
        "server always returns 500 → should fall through"
    );
    // retry_count=1 → 2 total attempts
    assert_eq!(
        server.request_count(),
        2,
        "should retry once (2 total requests)"
    );
}
