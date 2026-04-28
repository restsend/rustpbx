use super::test_ua::{TestUa, TestUaConfig};
use crate::config::ProxyConfig;
use crate::proxy::call::CallModule;
use crate::proxy::registrar::RegistrarModule;
use crate::proxy::server::SipServerBuilder;
use crate::proxy::user::MemoryUserBackend;
use crate::proxy::{ProxyAction, ProxyModule, locator_db::DbLocator};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::transaction::endpoint::MessageInspector;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipAddr;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct CapturedInvite {
    from_addr: SipAddr,
    request: rsipstack::sip::Request,
}

struct InviteCaptureInspector {
    captured: Arc<Mutex<Vec<CapturedInvite>>>,
}

impl MessageInspector for InviteCaptureInspector {
    fn before_send(
        &self,
        msg: rsipstack::sip::SipMessage,
        _dest: Option<&rsipstack::transport::SipAddr>,
    ) -> rsipstack::sip::SipMessage {
        msg
    }

    fn after_received(
        &self,
        msg: rsipstack::sip::SipMessage,
        from: &rsipstack::transport::SipAddr,
    ) -> rsipstack::sip::SipMessage {
        if let rsipstack::sip::SipMessage::Request(request) = &msg
            && request.method == rsipstack::sip::Method::Invite
            && request
                .to_header()
                .ok()
                .and_then(|to| to.tag().ok().flatten())
                .is_none()
        {
            let mut lock = self.captured.lock().expect("capture lock poisoned");
            lock.push(CapturedInvite {
                from_addr: from.clone(),
                request: request.clone(),
            });
        }

        msg
    }
}

struct RejectInviteModule;

impl RejectInviteModule {
    fn create(
        _server: Arc<crate::proxy::server::SipServerInner>,
        _config: Arc<ProxyConfig>,
    ) -> Result<Box<dyn ProxyModule>> {
        Ok(Box::new(Self))
    }
}

#[async_trait]
impl ProxyModule for RejectInviteModule {
    fn name(&self) -> &str {
        "reject_invite"
    }

    fn allow_methods(&self) -> Vec<rsipstack::sip::Method> {
        vec![rsipstack::sip::Method::Invite]
    }

    async fn on_start(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        _cookie: crate::call::TransactionCookie,
    ) -> Result<ProxyAction> {
        if tx.original.method == rsipstack::sip::Method::Invite {
            tx.reply(rsipstack::sip::StatusCode::NotFound).await.ok();
            return Ok(ProxyAction::Abort);
        }

        Ok(ProxyAction::Continue)
    }
}

async fn start_server_a(db_url: &str, port: u16, peer_port: u16) -> Result<Arc<crate::proxy::server::SipServer>> {
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec!["registrar".to_string(), "call".to_string()]),
        ensure_user: Some(false),
        cluster_peers: vec![format!("127.0.0.1:{}", peer_port)],
        ..Default::default()
    });

    let locator = DbLocator::new(db_url.to_string()).await?;
    let cancel = CancellationToken::new();

    let server = Arc::new(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(MemoryUserBackend::new(None)))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel)
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("call", |inner, config| Ok(Box::new(CallModule::new(config, inner))))
            .build()
            .await?,
    );

    let run = server.clone();
    tokio::spawn(async move {
        run.serve().await.ok();
    });

    Ok(server)
}

async fn start_server_b(
    db_url: &str,
    port: u16,
    peer_port: u16,
    capture: Arc<Mutex<Vec<CapturedInvite>>>,
) -> Result<Arc<crate::proxy::server::SipServer>> {
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec!["reject_invite".to_string(), "registrar".to_string()]),
        ensure_user: Some(false),
        cluster_peers: vec![format!("127.0.0.1:{}", peer_port)],
        ..Default::default()
    });

    let locator = DbLocator::new(db_url.to_string()).await?;
    let cancel = CancellationToken::new();

    let server = Arc::new(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(MemoryUserBackend::new(None)))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel)
            .with_message_inspector(Box::new(InviteCaptureInspector { captured: capture }))
            .register_module("reject_invite", RejectInviteModule::create)
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .build()
            .await?,
    );

    let run = server.clone();
    tokio::spawn(async move {
        run.serve().await.ok();
    });

    Ok(server)
}

async fn create_ua(username: &str, proxy_addr: SocketAddr, port: u16) -> Result<TestUa> {
    let config = TestUaConfig {
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: port,
        proxy_addr,
    };

    let mut ua = TestUa::new(config);
    ua.start().await?;
    ua.register().await?;
    Ok(ua)
}

#[tokio::test]
async fn test_cluster_home_proxy_request_shape_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let temp_db = NamedTempFile::new()?;
    let db_url = format!("sqlite:{}", temp_db.path().to_string_lossy());

    let port_a = portpicker::pick_unused_port().unwrap_or(16060);
    let port_b = portpicker::pick_unused_port().unwrap_or(16061);

    let captured = Arc::new(Mutex::new(Vec::<CapturedInvite>::new()));

    let server_a = start_server_a(&db_url, port_a, port_b).await?;
    let server_b = start_server_b(&db_url, port_b, port_a, captured.clone()).await?;

    sleep(Duration::from_millis(250)).await;

    let proxy_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse()?;
    let proxy_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse()?;

    let bp_port = portpicker::pick_unused_port().unwrap_or(26060);
    let lp_port = portpicker::pick_unused_port().unwrap_or(26061);

    let bp = create_ua("bp", proxy_a, bp_port).await?;
    let lp = create_ua("lp", proxy_b, lp_port).await?;

    sleep(Duration::from_millis(250)).await;

    let _ = tokio::time::timeout(
        Duration::from_secs(4),
        bp.make_call(
            "lp",
            Some(
                "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n"
                    .to_string(),
            ),
        ),
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(5);
    let captured_invite = loop {
        if let Some(item) = captured
            .lock()
            .expect("capture lock poisoned")
            .first()
            .cloned()
        {
            break item;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("did not capture A->B INVITE in time");
        }
        sleep(Duration::from_millis(50)).await;
    };

    let request = captured_invite.request;
    let req_uri = request.uri.clone();
    let from_uri = request
        .from_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .ok_or_else(|| anyhow::anyhow!("captured request missing From URI"))?;
    let to_uri = request
        .to_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .ok_or_else(|| anyhow::anyhow!("captured request missing To URI"))?;

    assert_eq!(req_uri.user().unwrap_or_default(), "lp");
    assert_eq!(to_uri.user().unwrap_or_default(), "lp");
    assert_eq!(from_uri.user().unwrap_or_default(), "bp");

    let req_uri_rendered = req_uri.to_string();
    assert!(
        !req_uri_rendered.contains(&format!(":{}", lp_port)),
        "Request-URI should not use lp contact port when routing via home proxy"
    );

    let request_line = format!("{} {} SIP/2.0", request.method, request.uri);
    println!("[cluster-e2e] A->B source transport: {}", captured_invite.from_addr);
    println!("[cluster-e2e] A->B request-line: {}", request_line);
    println!("[cluster-e2e] A->B From: {}", from_uri);
    println!("[cluster-e2e] A->B To: {}", to_uri);
    println!("[cluster-e2e] A->B full request:\n{}", request);

    bp.stop();
    lp.stop();
    server_a.stop();
    server_b.stop();

    Ok(())
}
