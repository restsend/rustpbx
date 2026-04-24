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

async fn create_test_ua(username: &str, proxy_addr: SocketAddr, local_port: u16) -> Result<TestUa> {
    let config = TestUaConfig {
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port,
        proxy_addr,
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    Ok(ua)
}

#[tokio::test]
#[ignore = "CallModule re-INVITE SDP answer not yet implemented; B2BUA path is covered by SipSession"]
async fn test_update_with_sdp_flow() {
    let _ = tracing_subscriber::fmt::try_init();

    let port = portpicker::pick_unused_port().unwrap_or(15065);
    let config = Arc::new(create_test_proxy_config(port));

    let user_backend = MemoryUserBackend::new(None);
    user_backend
        .create_user(SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    user_backend
        .create_user(SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

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
            Ok(Box::new(AuthModule::new(inner.clone(), inner.proxy_config.clone())))
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
    sleep(Duration::from_millis(200)).await;

    let proxy_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let alice = create_test_ua("alice", proxy_addr, 25061).await.unwrap();
    let bob = create_test_ua("bob", proxy_addr, 25062).await.unwrap();

    alice.register().await.unwrap();
    bob.register().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    // Alice calls Bob
    let offer_sdp = "v=0\r\no=- 123 456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_string();

    // Use a channel to get the call ID back from the spawned task to avoid deadlock
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let alice_clone = alice.clone();
    tokio::spawn(async move {
        let res = alice_clone.make_call("bob", Some(offer_sdp)).await;
        let _ = tx.send(res).await;
    });

    // Bob answers
    sleep(Duration::from_millis(500)).await;
    let bob_events = bob.process_dialog_events().await.unwrap();
    let mut bob_call_id = None;
    for event in bob_events {
        if let TestUaEvent::IncomingCall(id, _) = event {
            let answer_sdp = "v=0\r\no=- 456 789 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_string();
            bob.answer_call(&id, Some(answer_sdp)).await.unwrap();
            bob_call_id = Some(id);
            break;
        }
    }
    let _bob_call_id = bob_call_id.expect("Bob should receive incoming call");

    // Wait for Alice to get Call ID
    let alice_call_id = rx.recv().await.unwrap().expect("Alice call should succeed");

    // Wait for call to be established
    sleep(Duration::from_millis(500)).await;

    // Alice sends re-INVITE with new SDP (e.g. hold)
    // Spawn Bob's event processing in background so he can respond to the re-INVITE
    let bob_clone = bob.clone();
    let bob_handle = tokio::spawn(async move {
        // Process events for up to 5 seconds
        for _ in 0..50 {
            if let Ok(events) = bob_clone.process_dialog_events().await {
                for event in &events {
                    if let TestUaEvent::CallUpdated(_, method, _) = event
                        && *method == rsipstack::sip::Method::Invite {
                            info!("Bob's background task processed re-INVITE");
                            return true;
                        }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        false
    });

    info!("Alice sending re-INVITE with SDP");
    let hold_sdp = "v=0\r\no=- 123 457 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n".to_string();
    let alice_received_sdp = alice
        .send_reinvite(&alice_call_id, Some(hold_sdp.clone()))
        .await
        .unwrap();

    // Wait for Bob's background processing to complete
    let bob_processed = bob_handle.await.unwrap();
    info!("Bob processed re-INVITE in background: {}", bob_processed);

    // Verify Alice received an SDP answer in the 200 OK
    assert!(
        alice_received_sdp.is_some(),
        "Alice should receive SDP answer in 200 OK Response to re-INVITE"
    );
    info!(
        "Alice received SDP answer from Proxy: {:?}",
        alice_received_sdp
    );

    // Verify Bob processed the re-INVITE (already done in background task)
    assert!(
        bob_processed,
        "Bob should have received a forwarded re-INVITE request"
    );

    // Verify the SDP answer contains expected content
    let alice_sdp = alice_received_sdp.unwrap();
    assert!(
        alice_sdp.contains("PCMU/8000"),
        "SDP answer should contain codec information"
    );

    // Cleanup
    alice.hangup(&alice_call_id).await.ok();
    cancel_token.cancel();
}
