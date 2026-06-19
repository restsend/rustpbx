use super::test_ua::TestUaEvent;
use crate::config::MediaProxyMode;
use crate::rwi::gateway::{EventCacheEntry, RwiGateway};
use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::sleep;
use tracing::info;

struct DnEventCapture {
    events: Vec<String>,
    _rx: broadcast::Receiver<EventCacheEntry>,
}

impl DnEventCapture {
    fn collect(&mut self) {
        while let Ok(entry) = self._rx.try_recv() {
            let flat = &entry.event;
            if flat.event_type.contains("dn") || flat.event_type.contains("state") {
                self.events.push(flat.event_type.to_string());
            }
        }
    }

    fn has_event(&self, name: &str) -> bool {
        self.events.iter().any(|n| n == name)
    }
}

fn setup_gateway_with_capture() -> (
    Arc<RwLock<RwiGateway>>,
    broadcast::Receiver<EventCacheEntry>,
) {
    let (tx, rx) = broadcast::channel::<EventCacheEntry>(1000);
    let mut gw = RwiGateway::new();
    gw.set_webhook_tx(tx);
    (Arc::new(RwLock::new(gw)), rx)
}

fn pcmu_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
         o=- 12345 12345 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

#[tokio::test]
#[ignore = "DnStateChanged removed; use agent_state_changed instead"]
async fn test_dn_events_register_and_call_flow() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let (gw, rx) = setup_gateway_with_capture();

    let mut proxy_config = crate::config::ProxyConfig::default();
    proxy_config.media_proxy = MediaProxyMode::Auto;
    proxy_config.addr = "127.0.0.1".to_string();

    let port = portpicker::pick_unused_port().unwrap_or(15060);
    let base = super::test_helpers::test_proxy_config(port);
    proxy_config.addr = base.addr;
    proxy_config.udp_port = base.udp_port;
    proxy_config.tcp_port = base.tcp_port;
    proxy_config.tls_port = base.tls_port;
    proxy_config.ws_port = base.ws_port;
    proxy_config.useragent = base.useragent;
    proxy_config.modules = base.modules;
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;

    let config = Arc::new(proxy_config);

    let user_backend = crate::proxy::user::MemoryUserBackend::new(None);
    for user in super::test_helpers::standard_test_users() {
        user_backend.create_user(user).await?;
    }
    let locator = crate::proxy::locator::MemoryLocator::new();
    let cancel_token = tokio_util::sync::CancellationToken::new();

    use crate::proxy::server::SipServerBuilder;
    let (_cdr_capture, cdr_sender) = super::cdr_capture::CdrCapture::new();

    let builder = super::test_helpers::register_standard_modules(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone())
            .with_callrecord_sender(Some(cdr_sender))
            .with_rwi_gateway(gw.clone()),
    );

    let server = Arc::new(builder.build().await?);
    let _server_ref = server.get_inner();

    let cancel_token_clone = cancel_token.clone();
    let _server_handle = crate::utils::spawn(async move {
        tokio::select! {
            _ = cancel_token_clone.cancelled() => {}
            result = server.serve() => {
                if let Err(e) = result {
                    tracing::warn!("E2E test server error: {:?}", e);
                }
            }
        }
    });

    sleep(Duration::from_millis(300)).await;

    let proxy_addr = format!("127.0.0.1:{}", port).parse()?;

    let alice_port = portpicker::pick_unused_port().unwrap_or(25000);
    let alice_config = super::test_ua::TestUaConfig {
        username: "alice".to_string(),
        password: "password123".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: alice_port,
        proxy_addr,
    };
    let mut alice = super::test_ua::TestUa::new(alice_config);
    alice.start().await?;
    alice.register().await?;

    let bob_port = portpicker::pick_unused_port().unwrap_or(25001);
    let bob_config = super::test_ua::TestUaConfig {
        username: "bob".to_string(),
        password: "password456".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: bob_port,
        proxy_addr,
    };
    let mut bob = super::test_ua::TestUa::new(bob_config);
    bob.start().await?;
    bob.register().await?;

    sleep(Duration::from_millis(300)).await;

    let mut capture = DnEventCapture {
        events: vec![],
        _rx: rx,
    };
    capture.collect();

    assert!(
        capture.has_event("REGISTERED"),
        "REGISTERED event should be emitted after registration"
    );

    let alice_sdp = pcmu_sdp(portpicker::pick_unused_port().unwrap_or(30000));
    let bob_sdp = pcmu_sdp(portpicker::pick_unused_port().unwrap_or(30001));

    let caller_handle = {
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        crate::utils::spawn(async move { a.make_call("bob", Some(sdp)).await })
    };

    sleep(Duration::from_millis(200)).await;

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the call");

    capture.collect();
    assert!(
        capture.has_event("DIALING"),
        "DIALING event should be emitted when INVITE is sent"
    );

    if let Some(ref id) = bob_dialog_id {
        bob.send_ringing(id, None).await?;
    }
    sleep(Duration::from_millis(200)).await;

    capture.collect();
    assert!(
        capture.has_event("RINGING"),
        "RINGING event should be emitted when callee rings"
    );

    if let Some(ref id) = bob_dialog_id {
        bob.answer_call(id, Some(bob_sdp.clone())).await?;
    }

    let _alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => {
            info!("Call established: {}", id);
            Some(id)
        }
        other => {
            info!("Caller result: {:?}", other);
            None
        }
    };

    sleep(Duration::from_millis(300)).await;

    capture.collect();
    assert!(
        capture.has_event("ESTABLISHED"),
        "ESTABLISHED event should be emitted when call is answered"
    );

    if let Some(ref id) = bob_dialog_id {
        bob.hangup(id).await?;
    }
    sleep(Duration::from_millis(500)).await;

    capture.collect();
    assert!(
        capture.has_event("ONHOOK"),
        "ONHOOK event should be emitted when a party hangs up"
    );
    assert!(
        capture.has_event("RELEASED"),
        "RELEASED event should be emitted when call is released"
    );

    cancel_token.cancel();
    sleep(Duration::from_millis(100)).await;
    Ok(())
}

#[tokio::test]
#[ignore = "DnStateChanged removed; use agent_state_changed instead"]
async fn test_dn_events_abandoned_on_reject() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let (gw, rx) = setup_gateway_with_capture();

    let port = portpicker::pick_unused_port().unwrap_or(15060);
    let mut proxy_config = super::test_helpers::test_proxy_config(port);
    proxy_config.media_proxy = MediaProxyMode::Auto;
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    let config = Arc::new(proxy_config);

    let user_backend = crate::proxy::user::MemoryUserBackend::new(None);
    for user in super::test_helpers::standard_test_users() {
        user_backend.create_user(user).await?;
    }
    let locator = crate::proxy::locator::MemoryLocator::new();
    let cancel_token = tokio_util::sync::CancellationToken::new();

    use crate::proxy::server::SipServerBuilder;
    let (_cdr_capture, cdr_sender) = super::cdr_capture::CdrCapture::new();

    let builder = super::test_helpers::register_standard_modules(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel_token.clone())
            .with_callrecord_sender(Some(cdr_sender))
            .with_rwi_gateway(gw.clone()),
    );

    let server = Arc::new(builder.build().await?);
    let _server_ref = server.get_inner();

    let cancel_token_clone = cancel_token.clone();
    let _server_handle = crate::utils::spawn(async move {
        tokio::select! {
            _ = cancel_token_clone.cancelled() => {}
            result = server.serve() => {
                if let Err(e) = result {
                    tracing::warn!("E2E test server error: {:?}", e);
                }
            }
        }
    });

    sleep(Duration::from_millis(300)).await;

    let proxy_addr = format!("127.0.0.1:{}", port).parse()?;

    let alice_port = portpicker::pick_unused_port().unwrap_or(25010);
    let alice_config = super::test_ua::TestUaConfig {
        username: "alice".to_string(),
        password: "password123".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: alice_port,
        proxy_addr,
    };
    let mut alice = super::test_ua::TestUa::new(alice_config);
    alice.start().await?;
    alice.register().await?;

    let bob_port = portpicker::pick_unused_port().unwrap_or(25011);
    let bob_config = super::test_ua::TestUaConfig {
        username: "bob".to_string(),
        password: "password456".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: bob_port,
        proxy_addr,
    };
    let mut bob = super::test_ua::TestUa::new(bob_config);
    bob.start().await?;
    bob.register().await?;

    sleep(Duration::from_millis(300)).await;

    let alice_sdp = pcmu_sdp(portpicker::pick_unused_port().unwrap_or(30010));

    let _caller_handle = {
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        crate::utils::spawn(async move { a.make_call("bob", Some(sdp)).await })
    };

    sleep(Duration::from_millis(200)).await;

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the call");

    if let Some(ref id) = bob_dialog_id {
        bob.reject_call_with_reason(id, Some(486), Some("Busy Here".to_string()))
            .await?;
    }

    sleep(Duration::from_millis(500)).await;

    let mut capture = DnEventCapture {
        events: vec![],
        _rx: rx,
    };
    capture.collect();

    assert!(
        capture.has_event("ABANDONED"),
        "ABANDONED event should be emitted when callee rejects (486)"
    );

    cancel_token.cancel();
    sleep(Duration::from_millis(100)).await;
    Ok(())
}
