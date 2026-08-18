//! Cluster session-registry wiring e2E.
//!
//! Two servers (A, B) share one sqlite database used both as the shared
//! locator and as the `db` session-registry backend. A call placed through
//! A must publish its ownership record visible from BOTH nodes, and the
//! record must disappear after the call ends (RAII unregister).

use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transaction::endpoint::MessageInspector;
use rsipstack::transaction::transaction::Transaction;
use rustpbx::config::{ClusterConfig, ClusterPeer, ProxyConfig};
use rustpbx::proxy::call::CallModule;
use rustpbx::proxy::registrar::RegistrarModule;
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::user::MemoryUserBackend;
use rustpbx::proxy::{ProxyModule, locator_db::DbLocator};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Registrar-only module for node B: replies 404 to INVITEs so B never hosts
/// a session (any call that lands there terminates).
struct RejectInviteModule;

impl RejectInviteModule {
    fn create(
        _server: Arc<rustpbx::proxy::server::SipServerInner>,
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
        _cookie: rustpbx::call::TransactionCookie,
    ) -> Result<rustpbx::proxy::ProxyAction> {
        if tx.original.method == rsipstack::sip::Method::Invite {
            tx.reply(rsipstack::sip::StatusCode::NotFound).await.ok();
            return Ok(rustpbx::proxy::ProxyAction::Abort);
        }
        Ok(rustpbx::proxy::ProxyAction::Continue)
    }
}

/// No-op inspector (kept for builder parity with the home-proxy test).
struct NopInspector;

impl MessageInspector for NopInspector {
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
        _from: Option<&rsipstack::transport::SipAddr>,
    ) -> rsipstack::sip::SipMessage {
        msg
    }
}

async fn start_server(
    db_url: &str,
    port: u16,
    peer_port: u16,
    reject_invites: bool,
) -> Result<Arc<rustpbx::proxy::server::SipServer>> {
    let mut modules = vec!["registrar".to_string()];
    if reject_invites {
        modules.push("reject_invite".to_string());
    } else {
        modules.push("call".to_string());
    }
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(modules),
        ensure_user: Some(false),
        ..Default::default()
    });

    let peer: SocketAddr = format!("127.0.0.1:{}", peer_port).parse().unwrap();
    let self_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let peer_entry = |a: SocketAddr| ClusterPeer {
        addr: a.ip().to_string(),
        sip_port: a.port(),
        ami_port: 0,
    };
    // The cluster peer list includes every node (self included) — self
    // resolution matches the local listener against this list.
    let cluster = ClusterConfig {
        peers: vec![peer_entry(peer), peer_entry(self_addr)],
        session_registry_backend: "db".to_string(),
        ..Default::default()
    };

    let locator = DbLocator::new(db_url.to_string()).await?;
    let cancel = CancellationToken::new();

    // Shared-database connection for the db session-registry backend.
    let mut opt = sea_orm::ConnectOptions::new(db_url.to_string());
    opt.max_connections(1);
    let db = sea_orm::Database::connect(opt).await?;

    let mut builder = SipServerBuilder::new(config)
        .with_cluster_peers(vec![peer])
        .with_cluster_config(Some(cluster))
        .with_database_connection(db)
        .with_user_backend(Box::new(MemoryUserBackend::new(None)))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel)
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        });
    if reject_invites {
        builder = builder
            .register_module("reject_invite", RejectInviteModule::create)
            .with_message_inspector(Box::new(NopInspector));
    } else {
        builder = builder.register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });
    }

    let server = Arc::new(builder.build().await?);
    let run = server.clone();
    rustpbx::utils::spawn(async move {
        run.serve().await.ok();
    });
    Ok(server)
}

async fn create_ua(username: &str, proxy_addr: SocketAddr, port: u16) -> Result<TestUa> {
    let config = TestUaConfig {
        webrtc: false,
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
async fn test_cluster_session_registry_publishes_and_clears_call_ownership() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Shared sqlite for locator + session registry (the `db` backend).
    let temp_db = NamedTempFile::new()?;
    let db_url = format!("sqlite:{}", temp_db.path().to_string_lossy());

    // Run the full model migrations (proxy data init requires all tables;
    // cluster_sessions is part of the set).
    {
        let mut opt = sea_orm::ConnectOptions::new(db_url.clone());
        opt.max_connections(1);
        let db = sea_orm::Database::connect(opt).await?;
        use sea_orm_migration::MigratorTrait;
        rustpbx::models::migration::Migrator::up(&db, None).await?;
    }

    let port_a = portpicker::pick_unused_port().unwrap_or(16070);
    let port_b = portpicker::pick_unused_port().unwrap_or(16071);

    let server_a = start_server(&db_url, port_a, port_b, false).await?;
    let server_b = start_server(&db_url, port_b, port_a, true).await?;

    sleep(Duration::from_millis(250)).await;

    let proxy_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse()?;
    let expected_owner = format!("127.0.0.1:{}", port_a);

    let ua1_port = portpicker::pick_unused_port().unwrap_or(26070);
    let ua2_port = portpicker::pick_unused_port().unwrap_or(26071);
    let caller = create_ua("1001", proxy_a, ua1_port).await?;
    let callee = create_ua("1002", proxy_a, ua2_port).await?;

    sleep(Duration::from_millis(250)).await;

    // Place a call through node A (make_call resolves on dialog confirm, so
    // drive it in the background like the other proxy e2e tests).
    let offer_sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let caller_task = rustpbx::utils::spawn({
        let caller = caller.clone();
        let sdp = offer_sdp.clone();
        async move { caller.make_call("1002", Some(sdp)).await }
    });

    // Answer from the callee side.
    let answer_sdp = "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let mut answered = false;
    let mut caller_dialog_id = None;
    for _ in 0..50 {
        let events = callee.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                callee.answer_call(&id, Some(answer_sdp.clone())).await?;
                caller_dialog_id = Some(id);
                answered = true;
                break;
            }
        }
        if answered {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(answered, "callee should receive and answer the call");
    // The caller-side dialog id (B2BUA: distinct from the callee's).
    let dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_task).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(e),
        Ok(Err(e)) => return Err(anyhow::anyhow!("caller task failed: {e}")),
        Err(_) => anyhow::bail!("make_call did not resolve after answer"),
    };
    sleep(Duration::from_millis(400)).await;

    // Find the live session id on A.
    let entries = server_a.inner.active_call_registry.list_recent(10);
    assert!(!entries.is_empty(), "call must be registered on node A");
    let session_id = entries[0].session_id.clone();

    // Ownership record visible from BOTH nodes via the shared db backend.
    let owner_from_a = server_a
        .inner
        .session_registry
        .lookup_owner(&session_id)
        .await;
    assert_eq!(
        owner_from_a.as_deref(),
        Some(expected_owner.as_str()),
        "node A must see itself as owner"
    );
    let owner_from_b = server_b
        .inner
        .session_registry
        .lookup_owner(&session_id)
        .await;
    assert_eq!(
        owner_from_b.as_deref(),
        Some(expected_owner.as_str()),
        "node B must resolve the owner as node A"
    );

    // Full record fields round-trip.
    let info = server_b.inner.session_registry.lookup(&session_id).await;
    assert!(info.is_some(), "full SessionInfo must be resolvable from B");
    let info = info.unwrap();
    assert_eq!(info.direction, "inbound");
    assert!(!info.caller.is_empty() && !info.callee.is_empty());

    // End the call; the RAII guard unregisters (fire-and-forget spawn).
    tokio::time::timeout(Duration::from_secs(5), caller.hangup(&dialog_id)).await??;
    sleep(Duration::from_millis(800)).await;

    let owner_after = server_b
        .inner
        .session_registry
        .lookup_owner(&session_id)
        .await;
    assert!(
        owner_after.is_none(),
        "registry record must be cleared after call end (got {owner_after:?})"
    );

    caller.stop();
    callee.stop();
    server_a.stop();
    server_b.stop();

    Ok(())
}
