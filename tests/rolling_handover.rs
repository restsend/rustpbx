//! Rolling process handover e2e — two generations in one process.
//!
//! The predecessor (old release) and successor (new release) are built with
//! the REAL production components: `AppStateBuilder` (full AppState incl.
//! the rolling-pumped UDP transport), `create_router`, `app::run` and
//! `RollingRuntime::handle_termination` — the same call chain the SIGTERM
//! handler drives. The only test seams:
//!
//! * the SIGTERM signal itself is replaced by calling
//!   `handle_termination` directly (the builder merely forwards SIGTERM
//!   there in production),
//! * `shutdown::reset_for_tests()` clears the process-global draining flag
//!   between generations (production runs two processes, each with its own
//!   flag).
//!
//! Scenario:
//! 1. Old generation binds everything; alice→bob call establishes on it.
//! 2. Successor starts: build fails while the ports are held, hello-pings
//!    the predecessor, retries until the takeover.
//! 3. `docker stop` equivalent: `handle_termination` — the predecessor
//!    closes its public listeners (established WS/HTTP conns preserved),
//!    swaps UDP to the loopback relay, and the successor binds immediately.
//! 4. alice hangs up the OLD call: the BYE lands on the SUCCESSOR's socket,
//!    is relayed over loopback HTTP, the predecessor terminates the session
//!    and the 200 rides the relay back. Old drains to zero and exits.
//! 5. A fresh alice→bob call completes entirely on the successor.

use anyhow::Result;
use rustpbx::app::{create_router, AppStateBuilder};
use rustpbx::config::{Config, RollingConfig, UserBackendConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[path = "common/mod.rs"]
mod common;

/// The three rolling e2e tests each bind overlapping resource kinds (SIP/
/// HTTP/loopback ports probed at runtime) and run three full app stacks —
/// serialize them so port probes and CPU contention cannot flake the gate.
static E2E_SERIALIZER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use common::rtp_utils::RtpReceiver;
use common::test_helpers::{make_sdp, standard_test_users};
use common::test_ua::TestUaEvent;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// UDP-free port probe (`portpicker` probes TCP — a TCP-free port may still
/// be UDP-bound by the server, which corrupts the UA topology).
fn free_udp_port(avoid: &[u16]) -> u16 {
    loop {
        let p = portpicker::pick_unused_port().unwrap_or(25060);
        if avoid.contains(&p) {
            continue;
        }
        if std::net::UdpSocket::bind(("127.0.0.1", p)).is_ok() {
            // Probe socket dropped — port is free for the UA.
            return p;
        }
    }
}

fn gen_config(
    http_port: u16,
    sip_port: u16,
    predecessor_port: u16,
    rtp_base: u16,
    db_file: &std::path::Path,
) -> Config {
    Config {
        http_addr: format!("127.0.0.1:{}", http_port),
        database_url: format!("sqlite://{}?mode=rwc", db_file.display()),
        rolling: Some(RollingConfig {
            enabled: true,
            predecessor_port,
            // Exercise the registration-dump path in every rolling e2e run.
            handoff_registrations: true,
            ..Default::default()
        }),
        // Loopback must be AMI-allowed: the rolling e2e drives the AMI
        // forward path (hangup proxy) from the test process.
        ami: Some(rustpbx::config::AmiConfig {
            allows: Some(vec!["127.0.0.1".to_string()]),
        }),
        proxy: rustpbx::config::ProxyConfig {
            addr: "127.0.0.1".to_string(),
            udp_port: Some(sip_port),
            modules: Some(vec![
                "auth".to_string(),
                "registrar".to_string(),
                "call".to_string(),
            ]),
            user_backends: vec![UserBackendConfig::Memory {
                users: Some(standard_test_users()),
            }],
            media_proxy: rustpbx::config::MediaProxyMode::All,
            ensure_user: Some(false),
            ..Default::default()
        },
        rtp_start_port: Some(rtp_base),
        rtp_end_port: Some(rtp_base + 999),
        ..Default::default()
    }
}

async fn build_generation(
    cfg: &Config,
    rolling: &Arc<rustpbx::rolling::RollingRuntime>,
) -> Result<rustpbx::app::AppState> {
    let mut builder = AppStateBuilder::new().with_config(cfg.clone());
    builder.rolling = Some(rolling.clone());
    builder.build().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolling_handover_relays_live_call_and_serves_new_traffic() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let _serial = E2E_SERIALIZER.lock().await;

    let http_port = free_port();
    let sip_port: u16 = free_udp_port(&[]);
    let predecessor_port = free_port();
    let dir = tempfile::Builder::new()
        .prefix("rolling-e2e")
        .tempdir()
        .unwrap();

    let old_cfg = gen_config(
        http_port,
        sip_port,
        predecessor_port,
        22000,
        &dir.path().join("old.db"),
    );
    let new_cfg = gen_config(
        http_port,
        sip_port,
        predecessor_port,
        23500,
        &dir.path().join("new.db"),
    );

    let rolling_old =
        rustpbx::rolling::RollingRuntime::new(old_cfg.rolling.clone().unwrap(), &old_cfg.http_addr);
    let rolling_new =
        rustpbx::rolling::RollingRuntime::new(new_cfg.rolling.clone().unwrap(), &new_cfg.http_addr);

    // ── generation 1 (predecessor) ────────────────────────────────────────
    let state_old = build_generation(&old_cfg, &rolling_old).await?;
    let router_old = create_router(state_old.clone());
    let run_old = tokio::spawn(rustpbx::app::run(
        state_old.clone(),
        router_old.clone(),
    ));
    sleep(Duration::from_millis(500)).await;

    // ── live call on the old generation ───────────────────────────────────
    // E2eTestServer-style UAs point at the OLD generation's SIP port; they
    // are plain UDP peers and keep pointing at the same ip:port across the
    // handover (socket ownership moves, the address does not).
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{}", sip_port).parse()?;
    let make = |username: &str, local: u16| {
        let config = common::test_ua::TestUaConfig {
            webrtc: false,
            username: username.to_string(),
            password: if username == "alice" {
                "password123".to_string()
            } else {
                "password456".to_string()
            },
            realm: "127.0.0.1".to_string(),
            local_port: local,
            proxy_addr: server_addr,
        };
        config
    };
    let alice_port = free_udp_port(&[sip_port]);
    let bob_port = free_udp_port(&[sip_port, alice_port]);
    let mut alice = rustpbx_proxy_ua(make("alice", alice_port));
    alice.start().await?;
    alice.register().await?;
    let mut bob = rustpbx_proxy_ua(make("bob", bob_port));
    bob.start().await?;
    bob.register().await?;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sdp = make_sdp(caller_receiver.port()?);
    let callee_sdp = make_sdp(callee_receiver.port()?);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });
    let mut bob_dialog = None;
    for _ in 0..50 {
        for event in bob.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                bob_dialog = Some(id);
                break;
            }
        }
        if bob_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog.is_some(), "call must establish on the old generation");
    let alice_dialog = tokio::time::timeout(Duration::from_secs(8), caller)
        .await
        .map_err(|_| anyhow::anyhow!("caller timed out"))?
        .map_err(|e| anyhow::anyhow!("caller task join failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("caller failed: {e}"))?;
    sleep(Duration::from_millis(300)).await;
    let calls_before = state_old
        .sip_server()
        .inner
        .active_call_registry
        .count();
    assert_eq!(calls_before, 1, "exactly one live call on the predecessor");

    // ── generation 2 (successor): wait-for-ports + hello pinger ───────────
    rolling_new.start_hello_pinger();
    let new_state_task = tokio::spawn({
        let cfg = new_cfg.clone();
        let rolling = rolling_new.clone();
        async move {
            loop {
                match build_generation(&cfg, &rolling).await {
                    Ok(state) => return Ok(state),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg.contains("Failed to bind") || msg.contains("Address already in use")
                        {
                            sleep(Duration::from_millis(200)).await;
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }
    });

    // Give the hello pinger a moment to register with the predecessor.
    sleep(Duration::from_millis(1500)).await;
    assert!(
        rolling_old.successor_heartbeat_fresh(),
        "predecessor must have seen the successor heartbeat"
    );

    // ── docker stop equivalent ────────────────────────────────────────────
    let handover = tokio::spawn({
        let rolling = rolling_old.clone();
        let state = state_old.clone();
        let router = router_old.clone();
        async move { rolling.handle_termination(&state, router).await }
    });

    // The successor takes over as soon as the predecessor releases the
    // ports (mid-drain — the live call keeps running on the predecessor).
    let state_new = tokio::time::timeout(Duration::from_secs(20), new_state_task)
        .await
        .map_err(|_| anyhow::anyhow!("successor build timed out"))?
        .map_err(|e| anyhow::anyhow!("successor task join failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("successor build failed: {e}"))?;
    let router_new = create_router(state_new.clone());
    let run_new = tokio::spawn(rustpbx::app::run(state_new.clone(), router_new));
    sleep(Duration::from_millis(500)).await;
    assert!(
        rolling_old.is_predecessor_role(),
        "old generation must be in predecessor role"
    );

    // Registration handoff: alice/bob were registered on the OLD
    // generation's memory locator — the successor's locator must have been
    // preloaded with their bindings (before any re-REGISTER).
    let alice_uri = "sip:alice@127.0.0.1".parse::<rsipstack::sip::Uri>()?;
    let preloaded = state_new
        .sip_server()
        .inner
        .locator
        .lookup(&alice_uri)
        .await
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    assert!(
        preloaded,
        "registration handoff must preload the successor's locator"
    );

    // In-process seam: the successor must not inherit the predecessor's
    // process-global draining flag (two processes each own their own).
    rustpbx::shutdown::reset_for_tests();

    // ── BYE for the OLD call lands on the SUCCESSOR and is relayed ────────
    alice
        .hangup(&alice_dialog)
        .await
        .expect("BYE relay through the successor must complete the hangup");

    // The predecessor drains to zero and finishes the handover window.
    tokio::time::timeout(Duration::from_secs(20), handover)
        .await
        .map_err(|_| anyhow::anyhow!("handover did not complete"))?
        .map_err(|e| anyhow::anyhow!("handover error: {e}"))?;
    assert_eq!(
        state_old.sip_server().inner.active_call_registry.count(),
        0,
        "predecessor must have drained the relayed call"
    );

    // ── fresh call entirely on the successor ──────────────────────────────
    alice.register().await?;
    bob.register().await?;
    let caller2 = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });
    let mut bob_dialog2 = None;
    for _ in 0..50 {
        for event in bob.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                bob_dialog2 = Some(id);
                break;
            }
        }
        if bob_dialog2.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        bob_dialog2.is_some(),
        "successor must serve a brand-new call after the handover"
    );
    let alice_dialog2 = tokio::time::timeout(Duration::from_secs(8), caller2)
        .await
        .map_err(|_| anyhow::anyhow!("successor caller timed out"))?
        .map_err(|e| anyhow::anyhow!("successor caller task join failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("successor caller failed: {e}"))?;
    let _ = alice_dialog2;
    assert_eq!(
        state_new.sip_server().inner.active_call_registry.count(),
        1,
        "the new call lives on the successor"
    );

    // Cleanup
    if let Some(d) = bob_dialog2 {
        let _ = bob.hangup(&d).await;
    }

    // ── task-leak guard (rolling steady state) ────────────────────────────
    // After the handover settled and the fresh call ended, the surviving
    // rolling tasks are a FIXED residual set (successor outbound/inbound
    // pumps; the predecessor is gone). Idle-time task growth would mean a
    // leaking loop (reporter/eviction/flush that never exits).
    sleep(Duration::from_secs(5)).await;
    rustpbx::utils::reset_task_metrics();
    sleep(Duration::from_secs(5)).await;
    let steady_baseline = rustpbx::utils::active_task_count();
    sleep(Duration::from_secs(6)).await;
    let steady_final = rustpbx::utils::active_task_count();
    let drift = steady_final.saturating_sub(steady_baseline);
    assert!(
        drift <= 2,
        "rolling steady state leaked tasks: baseline={steady_baseline} final={steady_final} drift={drift}"
    );

    run_new.abort();
    run_old.abort();
    Ok(())
}

/// Small adapter so the test can build TestUa values without importing the
/// config struct through two module layers.
fn rustpbx_proxy_ua(
    config: common::test_ua::TestUaConfig,
) -> common::test_ua::TestUa {
    common::test_ua::TestUa::new(config)
}

// ── ACD gate: the eviction loop must NOT evict agent connections while a
// caller waits in a queue, and must evict them once the queue drains. ────
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_loop_gates_on_queue_waiters() -> Result<()> {
    use rustpbx::proxy::active_call_registry::ActiveCallContextMeta;

    let _serial = E2E_SERIALIZER.lock().await;

    let http_port = free_port();
    let sip_port = free_udp_port(&[]);
    let predecessor_port = free_port();
    let dir = tempfile::Builder::new()
        .prefix("rolling-gate")
        .tempdir()
        .unwrap();
    let cfg = gen_config(
        http_port,
        sip_port,
        predecessor_port,
        22000,
        &dir.path().join("gate.db"),
    );
    let rolling = rustpbx::rolling::RollingRuntime::new(
        cfg.rolling.clone().unwrap(),
        &cfg.http_addr,
    );
    let mut builder = AppStateBuilder::new().with_config(cfg.clone());
    builder.rolling = Some(rolling.clone());
    let state = builder.build().await?;

    // Two simulated WS agent connections: the close handles are exactly
    // what ws.rs registers for bridged WebSocket sessions.
    let (tx1, rx1) = std::sync::mpsc::channel::<()>();
    let (tx2, rx2) = std::sync::mpsc::channel::<()>();
    let a1: std::net::SocketAddr = "10.7.0.1:5060".parse()?;
    let a2: std::net::SocketAddr = "10.7.0.2:5060".parse()?;
    let t1 = tx1.clone();
    rolling.register_real_close("Ws", a1, Arc::new(move || { let _ = t1.send(()); }));
    rolling.register_real_close("Ws", a2, Arc::new(move || { let _ = tx2.send(()); }));

    // A queued caller: context_meta with a queue_id — the same marker the
    // queue app stamps (core static queues AND cc skill-group queues).
    let qcall = "q-call-gate";
    state
        .sip_server()
        .inner
        .active_call_registry
        .set_context_meta(
            qcall.to_string(),
            ActiveCallContextMeta {
                queue_id: Some("support".to_string()),
                ..Default::default()
            },
        );

    rolling.start_eviction_loop(&state);

    // Gate held: rounds fire while the queue has waiters — no evictions.
    sleep(Duration::from_millis(3500)).await;
    assert!(
        rx1.try_recv().is_err() && rx2.try_recv().is_err(),
        "queue waiters must keep agent connections alive"
    );

    // Queue drained: the next round evicts both agents.
    state
        .sip_server()
        .inner
        .active_call_registry
        .remove(qcall);
    sleep(Duration::from_millis(5500)).await;
    assert!(
        rx1.try_recv().is_ok() && rx2.try_recv().is_ok(),
        "agents must be evicted once the queue drained"
    );
    Ok(())
}

// ── AMI forward: a hangup for a dialog living on the PREDECESSOR arrives
// at the SUCCESSOR's AMI and is proxied to the predecessor's loopback
// listener, tearing the old call down. ───────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ami_forward_hangup_reaches_predecessor() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let _serial = E2E_SERIALIZER.lock().await;
    let http_port = free_port();
    let sip_port = free_udp_port(&[]);
    let predecessor_port = free_port();
    let dir = tempfile::Builder::new()
        .prefix("rolling-ami")
        .tempdir()
        .unwrap();

    let old_cfg = gen_config(
        http_port,
        sip_port,
        predecessor_port,
        22000,
        &dir.path().join("old.db"),
    );
    let new_cfg = gen_config(
        http_port,
        sip_port,
        predecessor_port,
        23500,
        &dir.path().join("new.db"),
    );

    let rolling_old =
        rustpbx::rolling::RollingRuntime::new(old_cfg.rolling.clone().unwrap(), &old_cfg.http_addr);
    let rolling_new =
        rustpbx::rolling::RollingRuntime::new(new_cfg.rolling.clone().unwrap(), &new_cfg.http_addr);

    let state_old = build_generation(&old_cfg, &rolling_old).await?;
    let router_old = create_router(state_old.clone());
    let run_old = tokio::spawn(rustpbx::app::run(state_old.clone(), router_old.clone()));
    sleep(Duration::from_millis(500)).await;

    // Live call on the old generation (same UA dance as the main e2e).
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{}", sip_port).parse()?;
    let make = |username: &str, local: u16| {
        common::test_ua::TestUaConfig {
            webrtc: false,
            username: username.to_string(),
            password: if username == "alice" {
                "password123".to_string()
            } else {
                "password456".to_string()
            },
            realm: "127.0.0.1".to_string(),
            local_port: local,
            proxy_addr: server_addr,
        }
    };
    let alice_port = free_udp_port(&[sip_port]);
    let bob_port = free_udp_port(&[sip_port, alice_port]);
    let mut alice = rustpbx_proxy_ua(make("alice", alice_port));
    alice.start().await?;
    alice.register().await?;
    let mut bob = rustpbx_proxy_ua(make("bob", bob_port));
    bob.start().await?;
    bob.register().await?;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sdp = make_sdp(caller_receiver.port()?);
    let callee_sdp = make_sdp(callee_receiver.port()?);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });
    let mut bob_dialog = None;
    for _ in 0..50 {
        for event in bob.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                bob_dialog = Some(id);
                break;
            }
        }
        if bob_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog.is_some(), "call must establish on the old generation");
    let alice_dialog = tokio::time::timeout(Duration::from_secs(8), caller)
        .await
        .map_err(|_| anyhow::anyhow!("caller timed out"))?
        .map_err(|e| anyhow::anyhow!("caller task join failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("caller failed: {e}"))?;

    // The AMI target: a dialog id that lives ONLY on the predecessor.
    let old_dialog_id = state_old
        .sip_server()
        .inner
        .dialog_layer
        .all_dialog_ids()
        .first()
        .cloned()
        .expect("old generation must hold the call dialogs");

    // Successor takeover (hello → handover → bind), same as the main e2e.
    rolling_new.start_hello_pinger();
    let new_state_task = tokio::spawn({
        let cfg = new_cfg.clone();
        let rolling = rolling_new.clone();
        async move {
            loop {
                match build_generation(&cfg, &rolling).await {
                    Ok(state) => return Ok(state),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg.contains("Failed to bind") || msg.contains("Address already in use")
                        {
                            sleep(Duration::from_millis(200)).await;
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
        }
    });
    sleep(Duration::from_millis(1500)).await;
    let handover = tokio::spawn({
        let rolling = rolling_old.clone();
        let state = state_old.clone();
        let router = router_old.clone();
        async move { rolling.handle_termination(&state, router).await }
    });
    let state_new = tokio::time::timeout(Duration::from_secs(20), new_state_task)
        .await
        .map_err(|_| anyhow::anyhow!("successor build timed out"))?
        .map_err(|e| anyhow::anyhow!("successor task join failed: {e}"))?
        .map_err(|e| anyhow::anyhow!("successor build failed: {e}"))?;
    let run_new = tokio::spawn(rustpbx::app::run(state_new.clone(), create_router(state_new.clone())));
    sleep(Duration::from_millis(500)).await;
    rustpbx::shutdown::reset_for_tests();

    // ── AMI hangup on the SUCCESSOR for a PREDECESSOR-owned dialog ────────
    // The forward middleware must proxy it to the predecessor's loopback
    // listener; the predecessor tears the call down and the drain completes.
    let ami_path = "ami/v1"; // nested under the http router root
    let url = format!("http://127.0.0.1:{http_port}/{ami_path}/hangup/{old_dialog_id}");
    let resp = state_new.http_client.get(&url).timeout(Duration::from_secs(10)).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_success() && body.contains("hangup initiated"),
        "AMI forward must reach the predecessor (status={status}, body={body})"
    );

    tokio::time::timeout(Duration::from_secs(20), handover)
        .await
        .map_err(|_| anyhow::anyhow!("handover did not complete after AMI forward"))?
        .map_err(|e| anyhow::anyhow!("handover error: {e}"))?;
    assert_eq!(
        state_old.sip_server().inner.active_call_registry.count(),
        0,
        "forwarded hangup must have drained the predecessor"
    );
    let _ = alice_dialog;
    run_new.abort();
    run_old.abort();
    Ok(())
}

// ── TLS smoke: with certs configured, the rolling manual HTTPS accept
// loop (tokio-rustls path) must terminate TLS and serve the rolling
// endpoints — this is the WSS-bearing path every HTTPS deployment takes
// now that rolling is enabled by default. ────────────────────────────────
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_manual_accept_loop_serves_rolling_endpoints() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    // Production installs the ring provider in main_with_addons; the test
    // process builds the app directly and must install it before rustls.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let _serial = E2E_SERIALIZER.lock().await;

    // Self-signed cert via openssl (present on dev/CI images; the test
    // degrades to a no-op if the tooling is missing).
    let dir = tempfile::Builder::new()
        .prefix("rolling-tls")
        .tempdir()
        .unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    let gen_out = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            cert.to_str().unwrap(),
            "-days",
            "1",
            "-nodes",
            "-subj",
            "/CN=localhost",
        ])
        .output()?;
    if !gen_out.status.success() {
        eprintln!("openssl unavailable — skipping TLS smoke test");
        return Ok(());
    }

    let http_port = free_port();
    let https_port = free_port();
    let sip_port = free_udp_port(&[]);
    let predecessor_port = free_port();
    let mut cfg = gen_config(
        http_port,
        sip_port,
        predecessor_port,
        22000,
        &dir.path().join("tls.db"),
    );
    cfg.https_addr = Some(format!("127.0.0.1:{}", https_port));
    cfg.ssl_certificate = Some(cert.to_string_lossy().into_owned());
    cfg.ssl_private_key = Some(key.to_string_lossy().into_owned());

    let rolling =
        rustpbx::rolling::RollingRuntime::new(cfg.rolling.clone().unwrap(), &cfg.http_addr);
    let state = build_generation(&cfg, &rolling).await?;
    let router = create_router(state.clone());
    let run = tokio::spawn(rustpbx::app::run(state.clone(), router));
    sleep(Duration::from_millis(800)).await;

    // curl -k: skip the self-signed cert validation; we only need proof of
    // a working TLS handshake + HTTP/1.1 + rolling route behind it.
    let url = format!("https://127.0.0.1:{}/ami/v1/health", https_port);
    let out = std::process::Command::new("curl")
        .args(["-sk", "--max-time", "10", &url])
        .output()?;
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && body.contains("\"status\""),
        "manual TLS accept loop must serve HTTPS (body={body})"
    );

    // Takeover completed marker: the successor's own takeover also rides
    // the manual loop — a second GET proves the loop keeps serving.
    let out2 = std::process::Command::new("curl")
        .args(["-sk", "--max-time", "10", &format!("{url}?again=1")])
        .output()?;
    assert!(
        out2.status.success() && String::from_utf8_lossy(&out2.stdout).contains("\"status\""),
        "manual TLS accept loop must keep serving"
    );

    run.abort();
    Ok(())
}
