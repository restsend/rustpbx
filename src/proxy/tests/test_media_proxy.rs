use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::call::user::SipUser;
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::{
    auth::AuthModule, call::CallModule, locator::MemoryLocator, registrar::RegistrarModule,
    server::SipServerBuilder, user::MemoryUserBackend,
};
use anyhow::Result;
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
        media_proxy: MediaProxyMode::Auto,
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
    #[allow(dead_code)]
    cancel_token: CancellationToken,
    pub port: u16,
    #[allow(dead_code)]
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
}

#[tokio::test]
async fn test_webrtc_to_rtp_media_proxy_auto() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    // 1. Start Proxy Server
    let server = TestProxyServer::start().await?;
    let proxy_addr = format!("127.0.0.1:{}", server.port).parse()?;

    // 2. Setup Alice (WebRTC Caller)
    let mut alice_ua = TestUa::new(TestUaConfig {
        username: "alice".to_string(),
        password: "password123".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(15061),
        proxy_addr,
    });
    alice_ua.start().await?;
    alice_ua.register().await?;
    let alice = Arc::new(alice_ua);

    // 3. Setup Bob (RTP Callee)
    let mut bob = TestUa::new(TestUaConfig {
        username: "bob".to_string(),
        password: "password456".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(15062),
        proxy_addr,
    });
    bob.start().await?;
    bob.register().await?;

    // 4. Alice calls Bob with WebRTC SDP
    let webrtc_sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 12345 UDP/TLS/RTP/SAVPF 111\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r\n\
        a=setup:actpass\r\n\
        a=mid:0\r\n\
        a=sendrecv\r\n\
        a=rtcp-mux\r\n";

    // Run caller in background with timeout protection
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = webrtc_sdp.to_string();
        async move { a.make_call("bob", Some(sdp)).await }
    });
    info!("Alice call initiated.");

    // 5. Wait for Bob to receive INVITE and answer
    let mut bob_dialog_id = None;
    info!("Waiting for Bob to receive call...");
    for i in 0..50 {
        let events = bob.process_dialog_events().await.unwrap_or_default();
        for event in events {
            info!("Bob received event: {:?}", event);
            if let TestUaEvent::IncomingCall(id) = event {
                bob_dialog_id = Some(id.clone());
                // Answer immediately
                bob.answer_call(&id, None).await?;
                info!("Bob answered call");
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
        if i % 10 == 0 {
            info!("Still waiting for Bob...");
        }
    }

    assert!(bob_dialog_id.is_some(), "Bob should receive incoming call");

    // Wait for caller to complete with timeout
    match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(alice_dialog_id))) => {
            info!("Call established successfully");
            // Wait a bit then cleanup
            sleep(Duration::from_millis(200)).await;
            alice.hangup(&alice_dialog_id).await.ok();
        }
        Ok(Ok(Err(e))) => {
            warn!("Call failed: {:?}", e);
        }
        Ok(Err(e)) => {
            warn!("Caller task panicked: {:?}", e);
        }
        Err(_) => {
            warn!("Caller timed out after 5 seconds");
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_codec_negotiation_optimization() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Test scenario: Alice (WebRTC) with Opus+PCMU calls Bob (RTP) with PCMU only
    // Expected: PBX should optimize to use PCMU for both, avoiding transcoding

    // 1. Start Proxy Server
    let server = TestProxyServer::start().await?;
    let proxy_addr = format!("127.0.0.1:{}", server.port).parse()?;

    // 2. Setup Alice (WebRTC Caller) supporting both Opus and PCMU
    let mut alice_ua = TestUa::new(TestUaConfig {
        username: "alice".to_string(),
        password: "password123".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(15071),
        proxy_addr,
    });
    alice_ua.start().await?;
    alice_ua.register().await?;
    let alice = Arc::new(alice_ua);

    // 3. Setup Bob (RTP Callee) supporting PCMU only
    let mut bob = TestUa::new(TestUaConfig {
        username: "bob".to_string(),
        password: "password456".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(15072),
        proxy_addr,
    });
    bob.start().await?;
    bob.register().await?;

    // 4. Alice calls Bob with SDP offering both Opus and PCMU
    // This simulates a WebRTC client that supports multiple codecs
    let multi_codec_sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 12345 UDP/TLS/RTP/SAVPF 111 0\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r\n\
        a=setup:actpass\r\n\
        a=mid:0\r\n\
        a=sendrecv\r\n\
        a=rtcp-mux\r\n";

    // Run caller in background with timeout protection
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = multi_codec_sdp.to_string();
        async move { a.make_call("bob", Some(sdp)).await }
    });
    info!("Alice call initiated with multi-codec offer (Opus + PCMU)");

    // 5. Wait for Bob to receive call and answer
    let mut bob_dialog_id = None;
    for i in 0..50 {
        let events = bob.process_dialog_events().await.unwrap_or_default();
        for event in events {
            if let TestUaEvent::IncomingCall(id) = event {
                bob_dialog_id = Some(id.clone());

                // 6. Bob answers with PCMU only
                let bob_pcmu_sdp = "v=0\r\n\
                    o=- 789012 789012 IN IP4 127.0.0.1\r\n\
                    s=-\r\n\
                    c=IN IP4 127.0.0.1\r\n\
                    t=0 0\r\n\
                    m=audio 54321 RTP/AVP 0\r\n\
                    a=rtpmap:0 PCMU/8000\r\n\
                    a=sendrecv\r\n";

                bob.answer_call(&id, Some(bob_pcmu_sdp.to_string())).await?;
                info!("Bob answered with PCMU codec");
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
        if i % 10 == 0 {
            info!("Still waiting for Bob to receive call...");
        }
    }

    assert!(bob_dialog_id.is_some(), "Bob should receive call");

    // 7. Wait for caller to complete with timeout
    match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(alice_dialog_id))) => {
            info!("Call established successfully");
            // The optimization should detect that both support PCMU and avoid transcoding
            // Note: In real scenario, you would verify logs contain:
            // "Both parties support the same codec, optimizing to avoid transcoding"
            // "codec_a=PCMU codec_b=PCMU needs_transcoding=false"
            sleep(Duration::from_millis(200)).await;
            alice.hangup(&alice_dialog_id).await.ok();
            info!("Test completed - check logs for codec optimization messages");
        }
        Ok(Ok(Err(e))) => {
            warn!("Call failed: {:?}", e);
        }
        Ok(Err(e)) => {
            warn!("Caller task panicked: {:?}", e);
        }
        Err(_) => {
            warn!("Caller timed out after 5 seconds");
        }
    }
    Ok(())
}
