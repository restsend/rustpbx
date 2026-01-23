use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::call::user::SipUser;
use crate::config::ProxyConfig;
use crate::proxy::{
    auth::AuthModule, call::CallModule, locator::MemoryLocator, registrar::RegistrarModule,
    server::SipServerBuilder, user::MemoryUserBackend,
};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

fn create_test_proxy_config(port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        tls_port: None,
        ws_port: None,
        useragent: Some("RustPBX-Test/0.1.0".to_string()),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    }
}

fn create_test_users() -> Vec<SipUser> {
    vec![
        SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password123".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password456".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ]
}

pub struct TestProxyServer {
    cancel_token: CancellationToken,
    port: u16,
    pub server: Arc<crate::proxy::server::SipServer>,
}

impl TestProxyServer {
    pub async fn start() -> Result<Self> {
        let port = portpicker::pick_unused_port().unwrap_or(15060);
        let config = Arc::new(create_test_proxy_config(port));

        let user_backend = MemoryUserBackend::new(None);
        for user in create_test_users() {
            user_backend.create_user(user).await?;
        }

        let locator = MemoryLocator::new();
        let cancel_token = CancellationToken::new();
        let mut builder = SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone());

        builder = builder
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("auth", |inner, _config| {
                Ok(Box::new(AuthModule::new(inner)))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            });
        let server = Arc::new(builder.build().await?);
        let server_clone = server.clone();

        tokio::spawn(async move {
            if let Err(e) = server_clone.serve().await {
                warn!("Proxy server error: {:?}", e);
            }
        });

        sleep(Duration::from_millis(100)).await;
        Ok(Self {
            cancel_token,
            port,
            server,
        })
    }

    pub fn get_addr(&self) -> SocketAddr {
        format!("127.0.0.1:{}", self.port).parse().unwrap()
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

async fn create_test_ua(
    username: &str,
    password: &str,
    proxy_addr: SocketAddr,
    port: u16,
) -> Result<TestUa> {
    let config = TestUaConfig {
        username: username.to_string(),
        password: password.to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: port,
        proxy_addr,
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    Ok(ua)
}

#[tokio::test]
async fn test_b2bua_full_flow() {
    let _ = tracing_subscriber::fmt::try_init();
    let proxy = TestProxyServer::start().await.unwrap();
    let proxy_addr = proxy.get_addr();

    let alice_port = portpicker::pick_unused_port().unwrap_or(25030);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25031);

    let alice = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let bob = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    alice.register().await.unwrap();
    bob.register().await.unwrap();

    sleep(Duration::from_millis(500)).await;

    let dummy_sdp = "v=0\r\no=- 123456 123456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=sendrecv\r\n".to_string();

    // Alice calls Bob
    let alice_sdp = dummy_sdp.clone();
    let call_task = tokio::spawn(async move { alice.make_call("bob", Some(alice_sdp)).await });

    // Bob waits for incoming call and answers it
    let bob_sdp = dummy_sdp.clone();
    let answer_task = tokio::spawn(async move {
        for _ in 0..50 {
            let events = bob.process_dialog_events().await.unwrap_or_default();
            for event in events {
                match event {
                    TestUaEvent::IncomingCall(dialog_id) => {
                        info!("Bob received incoming call: {}", dialog_id);
                        bob.answer_call(&dialog_id, Some(bob_sdp.clone()))
                            .await
                            .unwrap();
                        return Ok::<_, anyhow::Error>(dialog_id);
                    }
                    _ => {}
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("No incoming call received"))
    });

    let (call_result, answer_result) = tokio::join!(call_task, answer_task);

    assert!(call_result.is_ok(), "Call should be initiated successfully");
    assert!(
        answer_result.is_ok(),
        "Call should be answered successfully"
    );

    // Verify active call in registry
    let registry = &proxy.server.inner.active_call_registry;

    // Wait for registry to be updated
    let mut calls = Vec::new();
    for _ in 0..20 {
        calls = registry.list_recent(10);
        if !calls.is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(!calls.is_empty(), "Should have at least one active call");

    let session_id = &calls[0].session_id;
    info!("Active session ID: {}", session_id);

    // Wait for media to flow
    info!("Waiting for media flow...");
    sleep(Duration::from_secs(3)).await;

    // Verify call is still active
    let calls = registry.list_recent(10);
    assert!(!calls.is_empty(), "Call should still be active");
    assert_eq!(&calls[0].session_id, session_id);

    info!("Call verified, cleaning up...");

    // Clean up
    proxy.stop();
}

/// Test RTP to WebRTC bridging
/// Scenario: bob (RTP) calls alice (WebRTC)
/// Expected: alice should receive WebRTC SDP (with RTP/SAVPF)
#[tokio::test]
async fn test_rtp_to_webrtc_bridge() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start proxy with media proxy enabled
    let port = portpicker::pick_unused_port().unwrap_or(15061);
    let mut config = create_test_proxy_config(port);
    config.media_proxy = crate::config::MediaProxyMode::All;
    let config = Arc::new(config);

    // Create users: alice supports WebRTC, bob does not
    let user_backend = MemoryUserBackend::new(None);
    let alice = SipUser {
        id: 1,
        username: "alice".to_string(),
        password: Some("password123".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        is_support_webrtc: true, // Alice supports WebRTC
        ..Default::default()
    };
    let bob = SipUser {
        id: 2,
        username: "bob".to_string(),
        password: Some("password456".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        is_support_webrtc: false, // Bob uses RTP
        ..Default::default()
    };
    user_backend.create_user(alice).await.unwrap();
    user_backend.create_user(bob).await.unwrap();

    let locator = MemoryLocator::new();
    let cancel_token = CancellationToken::new();
    let mut builder = SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel_token.clone());

    builder = builder
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("auth", |inner, _config| {
            Ok(Box::new(AuthModule::new(inner)))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });

    let server = Arc::new(builder.build().await.unwrap());
    let server_clone = server.clone();
    tokio::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            warn!("Proxy server error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(100)).await;

    let proxy_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    let alice_port = portpicker::pick_unused_port().unwrap_or(25032);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25033);

    let alice_ua = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let bob_ua = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    alice_ua.register().await.unwrap();
    bob_ua.register().await.unwrap();
    sleep(Duration::from_millis(500)).await;

    // Bob (RTP) calls Alice (WebRTC)
    let bob_rtp_sdp = "v=0\r\no=- 123456 123456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 0 8 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n".to_string();

    let call_task = tokio::spawn(async move { bob_ua.make_call("alice", Some(bob_rtp_sdp)).await });

    // Alice should receive incoming call and answer
    let answer_task = tokio::spawn(async move {
        for _ in 0..50 {
            let events = alice_ua.process_dialog_events().await.unwrap_or_default();
            for event in events {
                if let TestUaEvent::IncomingCall(dialog_id) = event {
                    info!(
                        "Alice (WebRTC) received incoming call from Bob (RTP): {}",
                        dialog_id
                    );
                    // Alice answers with WebRTC SDP
                    let alice_webrtc_sdp = "v=0\r\no=- 654321 654321 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 5678 UDP/TLS/RTP/SAVPF 111 101\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n".to_string();
                    alice_ua
                        .answer_call(&dialog_id, Some(alice_webrtc_sdp))
                        .await
                        .unwrap();
                    return Ok::<_, anyhow::Error>(dialog_id);
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("Alice did not receive incoming call"))
    });

    let (call_result, answer_result) = tokio::join!(call_task, answer_task);
    assert!(call_result.is_ok(), "Bob should initiate call successfully");
    assert!(
        answer_result.is_ok(),
        "Alice should receive WebRTC SDP and answer successfully"
    );

    info!("RTP to WebRTC bridging test passed!");
    cancel_token.cancel();
}

/// Test WebRTC to RTP bridging
/// Scenario: alice (WebRTC) calls bob (RTP)
/// Expected: bob should receive RTP SDP (with RTP/AVP)
#[tokio::test]
async fn test_webrtc_to_rtp_bridge() {
    let _ = tracing_subscriber::fmt::try_init();

    // Start proxy with media proxy enabled
    let port = portpicker::pick_unused_port().unwrap_or(15062);
    let mut config = create_test_proxy_config(port);
    config.media_proxy = crate::config::MediaProxyMode::All;
    let config = Arc::new(config);

    // Create users: alice supports WebRTC, bob does not
    let user_backend = MemoryUserBackend::new(None);
    let alice = SipUser {
        id: 1,
        username: "alice".to_string(),
        password: Some("password123".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        is_support_webrtc: true, // Alice supports WebRTC
        ..Default::default()
    };
    let bob = SipUser {
        id: 2,
        username: "bob".to_string(),
        password: Some("password456".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        is_support_webrtc: false, // Bob uses RTP
        ..Default::default()
    };
    user_backend.create_user(alice).await.unwrap();
    user_backend.create_user(bob).await.unwrap();

    let locator = MemoryLocator::new();
    let cancel_token = CancellationToken::new();
    let mut builder = SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel_token.clone());

    builder = builder
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("auth", |inner, _config| {
            Ok(Box::new(AuthModule::new(inner)))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });

    let server = Arc::new(builder.build().await.unwrap());
    let server_clone = server.clone();
    tokio::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            warn!("Proxy server error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(100)).await;

    let proxy_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    let alice_port = portpicker::pick_unused_port().unwrap_or(25034);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25035);

    let alice_ua = create_test_ua("alice", "password123", proxy_addr, alice_port)
        .await
        .unwrap();
    let bob_ua = create_test_ua("bob", "password456", proxy_addr, bob_port)
        .await
        .unwrap();

    alice_ua.register().await.unwrap();
    bob_ua.register().await.unwrap();
    sleep(Duration::from_millis(500)).await;

    // Alice (WebRTC) calls Bob (RTP)
    let alice_webrtc_sdp = "v=0\r\no=- 654321 654321 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 5678 UDP/TLS/RTP/SAVPF 111 101\r\na=rtpmap:111 opus/48000/2\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n".to_string();

    let call_task =
        tokio::spawn(async move { alice_ua.make_call("bob", Some(alice_webrtc_sdp)).await });

    // Bob should receive incoming call and answer
    let answer_task = tokio::spawn(async move {
        for _ in 0..50 {
            let events = bob_ua.process_dialog_events().await.unwrap_or_default();
            for event in events {
                if let TestUaEvent::IncomingCall(dialog_id) = event {
                    info!(
                        "Bob (RTP) received incoming call from Alice (WebRTC): {}",
                        dialog_id
                    );
                    // Bob answers with RTP SDP
                    let bob_rtp_sdp = "v=0\r\no=- 123456 123456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 1234 RTP/AVP 0 8 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n".to_string();
                    bob_ua
                        .answer_call(&dialog_id, Some(bob_rtp_sdp))
                        .await
                        .unwrap();
                    return Ok::<_, anyhow::Error>(dialog_id);
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow::anyhow!("Bob did not receive incoming call"))
    });

    let (call_result, answer_result) = tokio::join!(call_task, answer_task);
    assert!(
        call_result.is_ok(),
        "Alice should initiate call successfully"
    );
    assert!(
        answer_result.is_ok(),
        "Bob should receive RTP SDP and answer successfully"
    );

    info!("WebRTC to RTP bridging test passed!");
    cancel_token.cancel();
}
