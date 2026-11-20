//! E2E presence tests – real SIP server with presence module, PUBLISH handling
use super::common::{create_test_request, create_transaction};
use super::e2e_test_server::E2eTestServer;
use crate::call::TransactionCookie;
use crate::config::MediaProxyMode;
use crate::proxy::ProxyAction;
use crate::proxy::ProxyModule;
use crate::proxy::presence::{PresenceModule, PresenceStatus};
use anyhow::Result;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Start an E2E presence server + return the PresenceModule for direct testing.
async fn setup_presence_e2e() -> Result<(E2eTestServer, Box<dyn ProxyModule>)> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = E2eTestServer::start_with_presence(MediaProxyMode::Auto).await?;
    let module = PresenceModule::create(
        server.server_ref.clone(),
        server.server_ref.proxy_config.load_full(),
    )?;
    Ok((server, module))
}

#[tokio::test]
async fn test_presence_e2e_away_with_detail() -> Result<()> {
    let (server, module) = setup_presence_e2e().await?;

    // Publish away:meeting PIDF from bob
    let mut publish = create_test_request(
        rsipstack::sip::Method::Publish,
        "bob",
        None,
        "127.0.0.1",
        None,
    );
    publish.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>away:meeting</note></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx, _) = create_transaction(publish).await;
    let result = module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await?;
    assert!(matches!(result, ProxyAction::Abort));

    sleep(Duration::from_millis(50)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert!(
        matches!(state.status, PresenceStatus::Away(ref d) if d == "meeting"),
        "expected Away(\"meeting\"), got {:?}",
        state.status
    );
    assert_eq!(state.note.as_deref(), Some("away:meeting"));

    let notify_body = crate::proxy::presence::build_pidf_body("bob", "127.0.0.1", &state);
    assert!(notify_body.contains("<note>away:meeting</note>"));
    assert!(notify_body.contains("<rpid:away/>"));
    assert!(!notify_body.contains("rpid:busy"));

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_presence_e2e_away_to_idle() -> Result<()> {
    let (server, module) = setup_presence_e2e().await?;

    // Publish away:lunch
    let mut away = create_test_request(
        rsipstack::sip::Method::Publish,
        "bob",
        None,
        "127.0.0.1",
        None,
    );
    away.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>away:lunch</note></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx, _) = create_transaction(away).await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await?;
    sleep(Duration::from_millis(50)).await;

    assert!(matches!(
        server.server_ref.presence_manager.get_state("bob").status,
        PresenceStatus::Away(ref d) if d == "lunch"
    ));

    // Publish idle
    let mut idle = create_test_request(
        rsipstack::sip::Method::Publish,
        "bob",
        None,
        "127.0.0.1",
        None,
    );
    idle.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf"><tuple id="presence"><status><basic>open</basic></status><note>idle</note></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx2, _) = create_transaction(idle).await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx2,
            TransactionCookie::default(),
        )
        .await?;
    sleep(Duration::from_millis(50)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert_eq!(state.status, PresenceStatus::Idle);
    assert_eq!(state.note.as_deref(), Some("idle"));

    server.stop();
    Ok(())
}

#[cfg(feature = "contact-center")]
#[tokio::test]
async fn test_presence_e2e_authenticated_publish_returns_committed_result() -> Result<()> {
    use crate::addons::cc::{agent::AgentRegistry, presence_hook::CcPresencePublishHook};
    use rsipstack::sip::SipMessage;
    use rsipstack::transaction::Transaction;
    use rsipstack::transaction::key::{TransactionKey, TransactionRole};
    use rsipstack::transport::{SipAddr, TransportEvent, channel::ChannelConnection};
    use sea_orm_migration::MigratorTrait;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    let (server, module) = setup_presence_e2e().await?;
    let db = sea_orm::Database::connect("sqlite::memory:").await?;
    crate::addons::cc::migration::Migrator::up(&db, None).await?;
    let registry = Arc::new(AgentRegistry::with_db(db.clone()));
    registry.register("bob".into(), vec![], 1).await?;
    server
        .server_ref
        .presence_manager
        .add_publish_hook(Arc::new(CcPresencePublishHook::new(registry, Some(db))));
    for (authenticated, opted_in, expects_result) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let mut request = create_test_request(
            rsipstack::sip::Method::Publish,
            "bob",
            None,
            "127.0.0.1",
            None,
        );
        if opted_in {
            request.headers.push(rsipstack::sip::Header::Other(
                "X-Restsend-Presence-Result".into(),
                "snapshot-v1".into(),
            ));
        }
        request.body = br#"<presence xmlns="urn:ietf:params:xml:ns:pidf"><tuple id="p"><status><basic>open</basic></status><note>idle</note></tuple></presence>"#.to_vec();
        let cookie = TransactionCookie::default();
        if authenticated {
            cookie.set_user(crate::call::SipUser {
                username: "bob".into(),
                ..Default::default()
            });
        }
        let (_, incoming) = mpsc::unbounded_channel();
        let (outgoing, mut wire) = mpsc::unbounded_channel();
        let addr: SipAddr = "127.0.0.1:5060".parse::<std::net::SocketAddr>()?.into();
        let connection =
            ChannelConnection::create_connection(incoming, outgoing, addr, None).await?;
        let key = TransactionKey::from_request(&request, TransactionRole::Server)?;
        let mut tx = Transaction::new_server(
            key,
            request,
            server.server_ref.endpoint.inner.clone(),
            Some(connection.into()),
        );
        module
            .on_transaction_begin(CancellationToken::new(), &mut tx, cookie)
            .await?;
        let event = tokio::time::timeout(Duration::from_secs(2), wire.recv()).await?;
        let Some(TransportEvent::Incoming(SipMessage::Response(response), _, _)) = event else {
            anyhow::bail!("expected a SIP PUBLISH response, got {event:?}");
        };
        assert_eq!(response.status_code, rsipstack::sip::StatusCode::OK);
        if expects_result {
            assert!(response.headers.iter().any(|h| matches!(h,
                rsipstack::sip::Header::ContentType(t)
                    if t.value() == "application/vnd.restsend.presence-result+json")));
            let receipt: serde_json::Value = serde_json::from_slice(&response.body)?;
            assert_eq!(receipt["agent_id"], "bob");
            assert_eq!(receipt["outcome"], "committed");
            assert_eq!(receipt["state"]["status"], "idle");
            assert!(receipt["state"]["transition_id"].as_i64().unwrap() > 0);
        } else {
            assert!(response.body.is_empty());
        }
    }
    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_presence_e2e_busy() -> Result<()> {
    let (server, module) = setup_presence_e2e().await?;

    let mut publish = create_test_request(
        rsipstack::sip::Method::Publish,
        "bob",
        None,
        "127.0.0.1",
        None,
    );
    publish.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:busy/></rpid:activities></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx, _) = create_transaction(publish).await;
    module
        .on_transaction_begin(
            CancellationToken::new(),
            &mut tx,
            TransactionCookie::default(),
        )
        .await?;
    sleep(Duration::from_millis(50)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert_eq!(state.status, PresenceStatus::Busy);

    let notify_body = crate::proxy::presence::build_pidf_body("bob", "127.0.0.1", &state);
    assert!(notify_body.contains("<rpid:busy/>"));
    assert!(!notify_body.contains("rpid:away"));

    server.stop();
    Ok(())
}
