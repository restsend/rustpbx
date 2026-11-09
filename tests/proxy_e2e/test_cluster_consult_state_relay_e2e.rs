use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::call::CallModule;
use rustpbx::proxy::registrar::RegistrarModule;
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::user::MemoryUserBackend;
use rustpbx::proxy::locator_db::DbLocator;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

/// Cross-node form of `test_consult_state_relay` (2026-11-06 M1 triage).
///
/// The PRD cluster delivers a cross-node leg through the callee's home
/// proxy: the caller's node keeps the session, the home node runs a cluster
/// child that adopts the root session id and dials the local endpoint. The
/// consult_state marker relay must survive BOTH hops (caller session →
/// home-node child → consulted endpoint). On prd-1106 the last hop died
/// with `dialog not found for message method=UPDATE` on the home node and
/// the consulted phone never saw the marker (B stuck on the help-call
/// label through the whole 3-way).
async fn start_server(
    db_url: &str,
    port: u16,
    peer_port: u16,
) -> Result<Arc<rustpbx::proxy::server::SipServer>> {
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec!["registrar".to_string(), "call".to_string()]),
        ensure_user: Some(false),
        ..Default::default()
    });

    let peer_addr: SocketAddr = format!("127.0.0.1:{}", peer_port).parse().unwrap();
    let locator = DbLocator::new(db_url.to_string()).await?;
    let cancel = CancellationToken::new();

    let server = Arc::new(
        SipServerBuilder::new(config)
            .with_cluster_peers(vec![peer_addr])
            .with_user_backend(Box::new(MemoryUserBackend::new(None)))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel)
            .register_module("registrar", |inner, config| {
                Ok(Box::new(RegistrarModule::new(inner, config)))
            })
            .register_module("call", |inner, config| {
                Ok(Box::new(CallModule::new(config, inner)))
            })
            .build()
            .await?,
    );

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
async fn test_cluster_consult_state_marker_relays_across_home_proxy_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let temp_db = NamedTempFile::new()?;
    let db_url = format!("sqlite:{}", temp_db.path().to_string_lossy());

    let port_a = portpicker::pick_unused_port().unwrap_or(16070);
    let port_b = portpicker::pick_unused_port().unwrap_or(16071);

    let server_a = start_server(&db_url, port_a, port_b).await?;
    let server_b = start_server(&db_url, port_b, port_a).await?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let proxy_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse()?;
    let proxy_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse()?;

    let ap_port = portpicker::pick_unused_port().unwrap_or(26070);
    let lp_port = portpicker::pick_unused_port().unwrap_or(26071);

    // The caller registers on node A; the consulted party registers on node
    // B — the leg must be delivered through lp's home proxy, which is the
    // production cross-node shape this regression broke.
    let ap = Arc::new(create_ua("ap", proxy_a, ap_port).await?);
    let lp = create_ua("lp", proxy_b, lp_port).await?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();

    let caller = tokio::spawn({
        let ap = ap.clone();
        let sdp = sdp.clone();
        async move {
            // Production consult-leg shape: the INVITE carries the CC UUI leg
            // intent + the root Session-ID, so the home node's cluster child
            // adopts the root session id (`inherited_child_session_id`) —
            // the ingredient the plain-call form lacks and the state the
            // PRD cluster was in when the marker relay broke.
            ap.make_call_with_headers(
                "lp",
                Some(sdp),
                vec![
                    (
                        "User-to-User".to_string(),
                        "self-dial;encoding=ascii;purpose=call-center;leg=consult;session-id=0199aaaa-bbbb-4ccc-8ddd-eeeeffff0000".to_string(),
                    ),
                    (
                        "Session-ID".to_string(),
                        "0199aaaa-bbbb-4ccc-8ddd-eeeeffff0000".to_string(),
                    ),
                ],
            )
            .await
        }
    });

    let mut lp_dialog_id = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while lp_dialog_id.is_none() && std::time::Instant::now() < deadline {
        let events = lp.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                lp_dialog_id = Some(id.clone());
                lp.answer_call(&id, Some(sdp.clone())).await?;
                break;
            }
        }
        if lp_dialog_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(lp_dialog_id.is_some(), "call must ring lp across the cluster");

    let ap_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    let ap_dialog_id = ap_dialog_id.expect("call must establish across the home proxy");

    // Merge-install edge: the caller's UA announces the local 3-way with a
    // body-less marker UPDATE. The home-node child must relay it to lp.
    ap.send_update_with_headers(
        &ap_dialog_id,
        vec![rsipstack::sip::Header::UserToUser(
            "consult-state=merged".into(),
        )],
    )
    .await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = None;
    while std::time::Instant::now() < deadline {
        let events = lp.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::ConsultStateUpdated(_, phase) = event {
                seen = Some(phase);
            }
        }
        if seen.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        seen.as_deref(),
        Some("merged"),
        "consult_state marker must relay across the cluster child to lp"
    );

    // Exit edge: the talk marker relays as well (transfer-completion demotion
    // rides the same path on the client side).
    ap.send_update_with_headers(
        &ap_dialog_id,
        vec![rsipstack::sip::Header::UserToUser(
            "consult-state=talk".into(),
        )],
    )
    .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut talk_seen = false;
    while std::time::Instant::now() < deadline {
        let events = lp.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::ConsultStateUpdated(_, phase) = event {
                if phase == "talk" {
                    talk_seen = true;
                }
            }
        }
        if talk_seen {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(talk_seen, "consult-state=talk must relay across the cluster too");

    ap.hangup(&ap_dialog_id).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ap.stop();
    lp.stop();
    server_a.stop();
    server_b.stop();

    Ok(())
}
