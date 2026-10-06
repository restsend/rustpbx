//! Dual-node recursive-fork guard e2e (regression for the home-hop fix in
//! `216daf64` — "stop recursive cluster forks and identify call owners").
//!
//! Topology: node A and node B, each running FULL registrar + call modules
//! over one shared SQLite locator, wired exactly like production
//! (`with_cluster_config` + derived peer sockets — see src/app.rs). Every
//! node's peer manifest includes its own listener so `cluster_self_addr`
//! resolves — the precondition for the home-hop filter. User `alice` is
//! registered on BOTH nodes; `bp` (registered on A) dials her twice:
//!
//! * node-addressed (`sip:alice@<A's addr>`) — a home hop is TERMINAL: only
//!   A's own registrations ring, B must never see the call. Pre-fix this
//!   forked cluster-wide and cascaded into an A<->B INVITE storm.
//! * canonical (`sip:alice@<realm>`) — legitimate cluster-wide fork: both
//!   UAs ring once, B sees exactly the one legitimate cross-node leg, and
//!   B's home-hop filter must stop the leg from re-forking back. Pre-fix
//!   B re-forked the full shared contact set and the nodes ping-ponged
//!   initial INVITEs until Max-Forwards bled out.

use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::transaction::endpoint::MessageInspector;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::call::CallModule;
use rustpbx::proxy::locator_db::DbLocator;
use rustpbx::proxy::registrar::RegistrarModule;
use rustpbx::proxy::server::SipServerBuilder;
use rustpbx::proxy::user::MemoryUserBackend;
use rustpbx::proxy::ProxyModule;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

const MINIMAL_SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

/// Counts unique INITIAL INVITEs received inbound, keyed by
/// (Call-ID, top Via header) so UDP retransmissions collapse while each NEW
/// forked hop (fresh Via branch) counts.
#[derive(Clone, Default)]
struct InviteCounter {
    seen: Arc<Mutex<HashSet<(String, String)>>>,
}

impl InviteCounter {
    fn count(&self) -> usize {
        self.seen.lock().expect("invite counter lock").len()
    }
}

#[async_trait]
impl MessageInspector for InviteCounter {
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
        if let rsipstack::sip::SipMessage::Request(request) = &msg
            && request.method == rsipstack::sip::Method::Invite
            && request
                .to_header()
                .ok()
                .and_then(|to| to.tag().ok().flatten())
                .is_none()
        {
            let call_id = request
                .call_id_header()
                .map(|h| h.value().to_string())
                .unwrap_or_default();
            let top_via = request
                .via_header()
                .map(|h| h.value().to_string())
                .unwrap_or_default();
            self.seen
                .lock()
                .expect("invite counter lock")
                .insert((call_id, top_via));
        }
        msg
    }
}

struct ClusterNodes {
    server_a: Arc<rustpbx::proxy::server::SipServer>,
    server_b: Arc<rustpbx::proxy::server::SipServer>,
    proxy_a: SocketAddr,
    counter_a: InviteCounter,
    counter_b: InviteCounter,
    /// Keeps the shared locator database alive until the topology drops.
    _db_file: NamedTempFile,
}

async fn start_node(
    db_url: &str,
    self_port: u16,
    peer_port: u16,
    inspector: InviteCounter,
) -> Result<Arc<rustpbx::proxy::server::SipServer>> {
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(self_port),
        modules: Some(vec!["registrar".to_string(), "call".to_string()]),
        ensure_user: Some(false),
        ..Default::default()
    });

    let locator = DbLocator::new(db_url.to_string()).await?;
    let cancel = CancellationToken::new();

    // Wire the cluster exactly like production does (src/app.rs): BOTH the
    // `[cluster]` config (arms `cluster_self_addr` resolution — the
    // home-hop fork guard's precondition) AND the derived peer sockets
    // (enable cluster routing). The manifest includes THIS node: the peer
    // entry matching the local listener IS this node's cluster address.
    let cluster = rustpbx::config::ClusterConfig {
        peers: vec![
            rustpbx::config::ClusterPeer {
                addr: "127.0.0.1".to_string(),
                sip_port: self_port,
                // AMI is not exercised in this topology.
                ami_port: 0,
            },
            rustpbx::config::ClusterPeer {
                addr: "127.0.0.1".to_string(),
                sip_port: peer_port,
                ami_port: 0,
            },
        ],
        ..Default::default()
    };

    let builder = SipServerBuilder::new(config)
        .with_cluster_config(Some(cluster))
        .with_cluster_peers(vec![
            format!("127.0.0.1:{}", self_port).parse::<SocketAddr>()?,
            format!("127.0.0.1:{}", peer_port).parse::<SocketAddr>()?,
        ])
        .with_user_backend(Box::new(MemoryUserBackend::new(None)))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel)
        .with_message_inspector(Box::new(inspector))
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });

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

async fn count_incoming_calls(ua: &TestUa, total: &mut usize) -> Result<usize> {
    // `process_dialog_events` DRAINS the pump queue — accumulate across polls
    // or earlier sightings vanish.
    let events = ua.process_dialog_events().await?;
    *total += events
        .iter()
        .filter(|e| matches!(e, TestUaEvent::IncomingCall(_, _)))
        .count();
    Ok(*total)
}

/// Boots the two-node shared-locator topology and registers `bp` on A plus
/// `alice` on BOTH nodes. `ua_port_base` must be unique per test — the
/// binary runs test fns in parallel and the UAs bind fixed ports.
/// Returns the handles needed for assertions.
async fn setup_cluster(ua_port_base: u16) -> Result<(ClusterNodes, TestUa, TestUa, TestUa)> {
    let temp_db = NamedTempFile::new()?;
    let db_url = format!("sqlite:{}", temp_db.path().to_string_lossy());

    let port_a = portpicker::pick_unused_port().unwrap_or(16070);
    let port_b = portpicker::pick_unused_port().unwrap_or(16071);

    let counter_a = InviteCounter::default();
    let counter_b = InviteCounter::default();

    let server_a = start_node(&db_url, port_a, port_b, counter_a.clone()).await?;
    let server_b = start_node(&db_url, port_b, port_a, counter_b.clone()).await?;

    sleep(Duration::from_millis(250)).await;

    let proxy_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse()?;
    let proxy_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse()?;

    let bp = create_ua("bp", proxy_a, ua_port_base).await?;
    // Same user on both nodes — the shared locator now holds two contacts.
    let alice_a = create_ua("alice", proxy_a, ua_port_base + 1).await?;
    let alice_b = create_ua("alice", proxy_b, ua_port_base + 2).await?;
    sleep(Duration::from_millis(250)).await;

    Ok((
        ClusterNodes {
            server_a,
            server_b,
            proxy_a,
            counter_a,
            counter_b,
            _db_file: temp_db,
        },
        bp,
        alice_a,
        alice_b,
    ))
}

async fn teardown(nodes: &ClusterNodes, uas: &[&TestUa]) {
    for ua in uas {
        ua.stop();
    }
    nodes.server_a.stop();
    nodes.server_b.stop();
}

/// After the fork settles, no further cluster hops may arrive — pre-fix the
/// A<->B ping-pong fans out to dozens of INVITEs within this window.
const SETTLE: Duration = Duration::from_millis(2000);

#[tokio::test]
async fn test_node_addressed_home_hop_is_terminal_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (nodes, bp, alice_a, alice_b) = setup_cluster(26270).await?;
    let (mut rings_a, mut rings_b) = (0usize, 0usize);

    // bp (on A) dials `sip:alice@<A's addr>` — addressed straight at node A:
    // a home hop. Terminal semantics: only A's own registration may ring.
    let _ = tokio::time::timeout(
        Duration::from_secs(6),
        bp.make_call("alice", Some(MINIMAL_SDP.to_string())),
    )
    .await;

    // The local registration must ring.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if count_incoming_calls(&alice_a, &mut rings_a).await? >= 1 {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("alice on node A was never rung — local home-hop fork broken");
        }
        sleep(Duration::from_millis(50)).await;
    }

    sleep(SETTLE).await;
    count_incoming_calls(&alice_a, &mut rings_a).await?;
    count_incoming_calls(&alice_b, &mut rings_b).await?;
    let inbound_a = nodes.counter_a.count();
    let inbound_b = nodes.counter_b.count();
    println!(
        "[fork-guard/node-hop] inbound — A: {inbound_a}, B: {inbound_b}; rings — alice@A: {rings_a}, alice@B: {rings_b}"
    );

    assert_eq!(
        inbound_a, 1,
        "node A must only ever see bp's original INVITE — more means the cluster fork looped back"
    );
    assert_eq!(
        inbound_b, 0,
        "a home-addressed hop is terminal: node B must never be involved"
    );
    assert_eq!(
        rings_a, 1,
        "alice's A-side registration must ring exactly once"
    );
    assert_eq!(
        rings_b, 0,
        "alice's B-side registration must NOT ring for a node-addressed call"
    );

    teardown(&nodes, &[&bp, &alice_a, &alice_b]).await;
    Ok(())
}

#[tokio::test]
async fn test_canonical_fork_reaches_both_nodes_without_loop_e2e() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let (nodes, bp, alice_a, alice_b) = setup_cluster(26370).await?;
    let (mut rings_a, mut rings_b) = (0usize, 0usize);

    // bp dials the CANONICAL address (`sip:alice@realm`, no node port) —
    // this is the production dial shape: the fork must reach BOTH nodes
    // exactly once and B's home hop must not fork back.
    let canonical = format!("sip:alice@{}", nodes.proxy_a.ip());
    let _ = tokio::time::timeout(
        Duration::from_secs(6),
        bp.make_call_to_uri(&canonical, Some(MINIMAL_SDP.to_string())),
    )
    .await;

    // Both registrations must ring exactly once.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if count_incoming_calls(&alice_a, &mut rings_a).await? >= 1
            && count_incoming_calls(&alice_b, &mut rings_b).await? >= 1
        {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "canonical fork did not reach both registrations (alice@A={}, alice@B={})",
                rings_a,
                rings_b
            );
        }
        sleep(Duration::from_millis(50)).await;
    }

    sleep(SETTLE).await;
    count_incoming_calls(&alice_a, &mut rings_a).await?;
    count_incoming_calls(&alice_b, &mut rings_b).await?;
    let inbound_a = nodes.counter_a.count();
    let inbound_b = nodes.counter_b.count();
    println!(
        "[fork-guard/canonical] inbound — A: {inbound_a}, B: {inbound_b}; rings — alice@A: {rings_a}, alice@B: {rings_b}"
    );

    assert_eq!(
        inbound_a, 1,
        "node A must only ever see bp's original INVITE — more means the cluster fork looped back"
    );
    assert_eq!(
        inbound_b, 1,
        "node B must receive exactly the one legitimate cross-node leg — more means a home hop re-forked recursively"
    );
    assert_eq!(
        rings_a, 1,
        "alice's A-side registration must ring exactly once"
    );
    assert_eq!(
        rings_b, 1,
        "alice's B-side registration must ring exactly once — re-rings mean the home hop forked recursively"
    );

    teardown(&nodes, &[&bp, &alice_a, &alice_b]).await;
    Ok(())
}
