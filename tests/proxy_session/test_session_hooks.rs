use anyhow::Result;
use async_trait::async_trait;
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::locator::MemoryLocator;
use rustpbx::proxy::proxy_call::session_hooks::{CallSessionContext, CallSessionHook};
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::user::MemoryUserBackend;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::common::test_helpers::register_standard_modules;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};

#[derive(Clone)]
struct RecordingHook {
    connected: Arc<Mutex<Vec<CallSessionContext>>>,
    ended: Arc<Mutex<Vec<CallSessionContext>>>,
}

#[async_trait]
impl CallSessionHook for RecordingHook {
    async fn on_call_connected(&self, ctx: &CallSessionContext) {
        self.connected.lock().await.push(ctx.clone());
    }

    async fn on_call_ended(
        &self,
        ctx: &CallSessionContext,
        _reason: Option<&rustpbx::callrecord::CallRecordHangupReason>,
        _duration_secs: u64,
    ) {
        self.ended.lock().await.push(ctx.clone());
    }
}

#[tokio::test]
async fn test_session_hook_connected_and_ended() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let port = portpicker::pick_unused_port().unwrap_or(15060);
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        media_proxy: rustpbx::config::MediaProxyMode::All,
        ..Default::default()
    });

    let user_backend = MemoryUserBackend::new(None);
    for u in [
        SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        },
    ] {
        user_backend.create_user(u).await?;
    }

    let connected: Arc<Mutex<Vec<CallSessionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let ended: Arc<Mutex<Vec<CallSessionContext>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<dyn CallSessionHook> = Arc::new(RecordingHook {
        connected: connected.clone(),
        ended: ended.clone(),
    });

    let builder = register_standard_modules(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(user_backend))
            .with_locator(Box::new(MemoryLocator::new()))
            .with_cancel_token(CancellationToken::new())
            .with_session_hook(hook),
    );

    let server = builder.build().await?;
    let proxy_addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse()?;
    let serve_task = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    sleep(Duration::from_millis(100)).await;

    let mut alice = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "alice".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(26000),
        proxy_addr,
    });
    alice.start().await?;
    alice.register().await?;

    let mut bob = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "bob".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(26001),
        proxy_addr,
    });
    bob.start().await?;
    bob.register().await?;

    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio 30001 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
        .to_string();

    let call_task = tokio::spawn({
        let a = alice;
        let s = sdp.clone();
        async move {
            let dialog = a.make_call("bob", Some(s)).await?;
            sleep(Duration::from_millis(800)).await;
            a.hangup(&dialog).await?;
            Ok::<_, anyhow::Error>(())
        }
    });

    let mut bob_dialog = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(sdp.clone())).await?;
                bob_dialog = Some(id);
                break;
            }
        }
        if bob_dialog.is_some() { break; }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog.is_some(), "Bob should receive call");

    let _ = tokio::time::timeout(Duration::from_secs(10), call_task).await;
    sleep(Duration::from_millis(500)).await;

    let connected_events = connected.lock().await;
    assert!(!connected_events.is_empty(), "on_call_connected should fire");
    assert!(
        connected_events[0].callee.contains("bob"),
        "callee should contain bob, got {}",
        connected_events[0].callee
    );

    let ended_events = ended.lock().await;
    assert!(!ended_events.is_empty(), "on_call_ended should fire after hangup");

    serve_task.abort();
    Ok(())
}
