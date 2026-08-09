//! E2E presence tests – real SIP server with presence module, PUBLISH handling
use super::common::{create_test_request, create_transaction};
use super::e2e_test_server::E2eTestServer;
use crate::call::TransactionCookie;
use crate::config::MediaProxyMode;
use crate::proxy::ProxyModule;
use crate::proxy::presence::{PresenceModule, PresenceStatus};
use crate::proxy::ProxyAction;
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
        server.server_ref.proxy_config.clone(),
    )?;
    Ok((server, module))
}

#[tokio::test]
async fn test_presence_e2e_away_with_detail() -> Result<()> {
    let (server, module) = setup_presence_e2e().await?;

    // Publish away:meeting PIDF from bob
    let mut publish = create_test_request(
        rsipstack::sip::Method::Publish, "bob", None, "127.0.0.1", None,
    );
    publish.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>away:meeting</note></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx, _) = create_transaction(publish).await;
    let result = module
        .on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default())
        .await?;
    assert!(matches!(result, ProxyAction::Abort));

    sleep(Duration::from_millis(50)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert!(
        matches!(state.status, PresenceStatus::Away(ref d) if d == "meeting"),
        "expected Away(\"meeting\"), got {:?}", state.status
    );
    assert_eq!(state.note.as_deref(), Some("away:meeting"));

    let notify_body =
        crate::proxy::presence::build_pidf_body("bob", "127.0.0.1", &state);
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
        rsipstack::sip::Method::Publish, "bob", None, "127.0.0.1", None,
    );
    away.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>away:lunch</note></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx, _) = create_transaction(away).await;
    module
        .on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default())
        .await?;
    sleep(Duration::from_millis(50)).await;

    assert!(matches!(
        server.server_ref.presence_manager.get_state("bob").status,
        PresenceStatus::Away(ref d) if d == "lunch"
    ));

    // Publish idle
    let mut idle = create_test_request(
        rsipstack::sip::Method::Publish, "bob", None, "127.0.0.1", None,
    );
    idle.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf"><tuple id="presence"><status><basic>open</basic></status><note>idle</note></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx2, _) = create_transaction(idle).await;
    module
        .on_transaction_begin(CancellationToken::new(), &mut tx2, TransactionCookie::default())
        .await?;
    sleep(Duration::from_millis(50)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert_eq!(state.status, PresenceStatus::Idle);
    assert_eq!(state.note.as_deref(), Some("idle"));

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_presence_e2e_busy() -> Result<()> {
    let (server, module) = setup_presence_e2e().await?;

    let mut publish = create_test_request(
        rsipstack::sip::Method::Publish, "bob", None, "127.0.0.1", None,
    );
    publish.body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:busy/></rpid:activities></tuple></presence>"#.as_bytes().to_vec();

    let (mut tx, _) = create_transaction(publish).await;
    module
        .on_transaction_begin(CancellationToken::new(), &mut tx, TransactionCookie::default())
        .await?;
    sleep(Duration::from_millis(50)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert_eq!(state.status, PresenceStatus::Busy);

    let notify_body =
        crate::proxy::presence::build_pidf_body("bob", "127.0.0.1", &state);
    assert!(notify_body.contains("<rpid:busy/>"));
    assert!(!notify_body.contains("rpid:away"));

    server.stop();
    Ok(())
}
