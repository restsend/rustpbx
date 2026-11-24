use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::acl::AclModule;
use rustpbx::proxy::call::CallModule;
use rustpbx::proxy::registrar::RegistrarModule;
use rustpbx::proxy::routing::TrunkConfig;
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::user::MemoryUserBackend;
use rustpbx::proxy::locator_db::DbLocator;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Multi-node contract for `session_id_live_reentry_adopt` (default ON).
///
/// Production shape (2026-11-24, CDR #373296): the G system re-dials INTO R
/// while R's previous leg of the same logical call is still bridged — the
/// INVITE carries the root id on `X-RustPBX-SessionID`. Two facts must hold:
///
/// 1. SAME-NODE re-entry (the structural overlap): the second INVITE keeps
///    the root id — the registry entry is replaced by the newer session
///    (latest-wins), asserted here via the entry's `started_at` advancing.
///    The concurrent-reuse back-off minted a fresh id here and severed the
///    correlation.
/// 2. CROSS-NODE re-entry: the root id is live on node A, the re-entering
///    INVITE lands on node B — adoption must succeed because the occupancy
///    check is NODE-LOCAL (`active_call_registry` is process-local). If
///    anyone ever "upgrades" the check to consult shared/DB state, this half
///    of the test dies (and with it, the cross-node form that worked in
///    production on 2026-11-23).
///
/// Usernames are unique per UA: the locator DB is shared between the nodes,
/// so duplicate usernames would overwrite each other's registration and
/// fork the call dispatch (test-harness artifact, not a product concern).
const ROOT: &str = "a859f71f1a805afa";
const CORRELATION_HEADER: &str = "X-RustPBX-SessionID";

async fn start_server(
    db_url: &str,
    port: u16,
    peer_port: u16,
) -> Result<Arc<rustpbx::proxy::server::SipServer>> {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        modules: Some(vec![
            "acl".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ensure_user: Some(false),
        ..Default::default()
    };
    // The G-side SBC trunk: inbound (direction unset), source 127.0.0.1
    // matches `inbound_hosts`, and it declares its own correlation header —
    // the trunk-level override that wins over the global default.
    config.trunks.insert(
        "g-sbc".to_string(),
        TrunkConfig {
            dest: "127.0.0.1:1".to_string(),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            session_id_header: Some(CORRELATION_HEADER.to_string()),
            ..Default::default()
        },
    );

    let peer_addr: SocketAddr = format!("127.0.0.1:{}", peer_port).parse().unwrap();
    let locator = DbLocator::new(db_url.to_string()).await?;
    let cancel = CancellationToken::new();

    let server = Arc::new(
        SipServerBuilder::new(config.into())
            .with_cluster_peers(vec![peer_addr])
            .with_user_backend(Box::new(MemoryUserBackend::new(None)))
            .with_locator(Box::new(locator))
            .with_cancel_token(cancel)
            .register_module("acl", |inner, config| AclModule::create(inner, config))
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

/// Caller places an INVITE carrying the root correlation header; the callee
/// UA answers the first IncomingCall. Returns once the caller dialog is up.
async fn place_call(
    caller: &Arc<TestUa>,
    callee: &TestUa,
    callee_user: &str,
    sdp: &str,
    headers: Vec<(String, String)>,
) -> Result<rsipstack::dialog::DialogId> {
    let caller_handle = {
        let caller = caller.clone();
        let callee_user = callee_user.to_string();
        let sdp = sdp.to_string();
        rustpbx::utils::spawn(async move {
            caller
                .make_call_with_headers(&callee_user, Some(sdp), headers)
                .await
        })
    };
    let mut answered = false;
    for _ in 0..50 {
        for event in callee.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _offer) = event {
                callee.answer_call(&id, Some(sdp.to_string())).await?;
                answered = true;
                break;
            }
        }
        if answered {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::ensure!(
        answered,
        "{callee_user} must receive the INVITE (no IncomingCall within 5s)"
    );
    Ok(tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("caller timed out waiting for the dialog"))?
        .map_err(|e| anyhow::anyhow!("spawn join failed: {e:#}"))?
        .map_err(|e| anyhow::anyhow!("call failed: {e:#}"))?)
}

async fn wait_for_registry_entry(
    registry: &Arc<rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry>,
    id: &str,
    timeout: Duration,
) -> Option<rustpbx::proxy::active_call_registry::ActiveProxyCallEntry> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(entry) = registry.get(id) {
            return Some(entry);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn test_cluster_reentry_adopts_root_same_node_and_cross_node_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let temp_db = NamedTempFile::new()?;
    let db_url = format!("sqlite:{}", temp_db.path().to_string_lossy());

    let port_a = portpicker::pick_unused_port().unwrap_or(16110);
    let port_b = portpicker::pick_unused_port().unwrap_or(16111);

    let server_a = start_server(&db_url, port_a, port_b).await?;
    let server_b = start_server(&db_url, port_b, port_a).await?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let proxy_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse()?;
    let proxy_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse()?;

    let ua_port = |seed: u16| portpicker::pick_unused_port().unwrap_or(seed);

    // Unique usernames per UA — the locator DB is shared, duplicates fork
    // the dispatch. Each caller UA carries at most ONE live dialog.
    let alice1 = Arc::new(create_ua("alice1", proxy_a, ua_port(26110)).await?);
    let alice2 = Arc::new(create_ua("alice2", proxy_a, ua_port(26112)).await?);
    let bob_a = create_ua("bob_a", proxy_a, ua_port(26114)).await?;
    let charlie_a = create_ua("charlie_a", proxy_a, ua_port(26116)).await?;
    let alice3 = Arc::new(create_ua("alice3", proxy_b, ua_port(26118)).await?);
    let bob_b = create_ua("bob_b", proxy_b, ua_port(26120)).await?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();
    let reentry_headers = vec![(CORRELATION_HEADER.to_string(), ROOT.to_string())];

    let registry_a = server_a.get_inner().active_call_registry.clone();
    let registry_b = server_b.get_inner().active_call_registry.clone();

    // ── Call 1: first entry into node A — adopts the root id (no occupancy).
    let call_1_id = place_call(&alice1, &bob_a, "bob_a", &sdp, reentry_headers.clone()).await?;
    let entry_1 = wait_for_registry_entry(&registry_a, ROOT, Duration::from_secs(5))
        .await
        .expect("node A must hold the root session id after the first entry");
    let started_at_1 = entry_1.started_at;

    // ── Call 2: SAME-NODE re-entry while call 1 is still bridged (the
    // structural overlap). The root id must be adopted again — the registry
    // entry is replaced by the newer session, observable as started_at
    // advancing. The pre-fix back-off minted a fresh id here and the entry
    // kept call 1's timestamp.
    let call_2_id = place_call(
        &alice2,
        &charlie_a,
        "charlie_a",
        &sdp,
        reentry_headers.clone(),
    )
    .await
    .map_err(|e| e.context("same-node re-entry INVITE failed"))?;
    let entry_2 = wait_for_registry_entry(&registry_a, ROOT, Duration::from_secs(5))
        .await
        .expect("node A must still hold the root key during dual-live overlap");
    assert!(
        entry_2.started_at > started_at_1,
        "same-node re-entry must ADOPT the root id (entry replaced by the newer session); \
         a fresh id here means the concurrent-reuse back-off regressed"
    );

    // ── Call 3: CROSS-NODE re-entry — the root id is live on node A, the
    // INVITE lands on node B. Adoption must succeed because the occupancy
    // check is node-local; a shared-state check would back off here.
    let call_3_id = place_call(&alice3, &bob_b, "bob_b", &sdp, reentry_headers.clone())
        .await
        .map_err(|e| e.context("cross-node re-entry INVITE failed"))?;
    wait_for_registry_entry(&registry_b, ROOT, Duration::from_secs(5))
        .await
        .expect(
            "node B must adopt the root id while node A holds it live — \
             the occupancy check must stay node-local",
        );

    // ── Teardown: the oldest session hangs up first (instance-guarded
    // removal must spare the newer one); after ALL hangups the key is gone.
    alice1.hangup(&call_1_id).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = registry_a.get(ROOT);
    alice2.hangup(&call_2_id).await?;
    alice3.hangup(&call_3_id).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while registry_a.get(ROOT).is_some() {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the root registry key must be removed after all hangups"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    info!("test_cluster_reentry_adopts_root_same_node_and_cross_node_e2e PASSED");
    Ok(())
}
