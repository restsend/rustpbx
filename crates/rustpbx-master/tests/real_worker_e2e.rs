//! Real-process e2e: in-process master + real `rustpbx` worker binary +
//! system `sipbot`-grade raw SIP clients. Verifies:
//!   1. worker boots under the master and flips to Active
//!   2. HTTP fd forwarding serves REST (`GET /`) through the master
//!   3. SIP UDP relay answers REGISTER (auth 401 round-trip)
//!   4. SIP TCP fd forwarding answers REGISTER
//!   5. rotation: gen1 drains/exits, gen2 serves HTTP + SIP immediately
//!
//! Skips (with a note) when the `rustpbx` binary has not been built.

use rustpbx_master::access::{Access, ControlStreamKind};
use rustpbx_master::config::ConfigSubset;
use rustpbx_master::supervisor::{spawn_reaper, SupCore};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::runtime::Handle;

fn rustpbx_bin() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/rustpbx");
    if p.is_file() { Some(p.canonicalize().unwrap()) } else { None }
}

struct Env {
    _dir: tempfile::TempDir,
    conf: PathBuf,
    sock: PathBuf,
    sip_udp: u16,
    sip_tcp: u16,
    http: u16,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn setup_env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let (sip_udp, sip_tcp, http) = (free_port(), free_port(), free_port());
    let conf = dir.path().join("config.toml");
    let db = dir.path().join("rustpbx.sqlite3");
    std::fs::write(
        &conf,
        format!(
            r#"
http_addr = "127.0.0.1:{http}"
log_level = "warn"
database_url = "sqlite://{}?mode=rwc"
rtp_start_port = 26000
rtp_end_port = 27000

[proxy]
modules = ["acl", "auth", "registrar", "call"]
addr = "127.0.0.1"
udp_port = {sip_udp}
tcp_port = {sip_tcp}
registrar_expires = 300

[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
id = 0
enabled = true
username = "sipbot"
password = "12345678"
"#,
            db.display(),
        ),
    )
    .unwrap();
    let sock = dir.path().join("master.sock");
    Env { _dir: dir, conf, sock, sip_udp, sip_tcp, http }
}

async fn boot_master(env: &Env, worker_bin: &Path) -> Arc<SupCore> {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let bind_ip = conf.bind_ip().unwrap();
    let udp = Access::bind_udp(bind_ip, &[env.sip_udp]).unwrap();
    let udp_map: HashMap<u16, Arc<tokio::net::UdpSocket>> =
        udp.iter().map(|(id, s)| (*id, s.clone())).collect();
    let mut core = SupCore::new(conf, &env.conf, &env.sock, udp_map).unwrap();
    // Set BEFORE the Arc is shared with access/ipc tasks.
    core.set_worker_bin(PathBuf::from(worker_bin));
    let access = Access::new(core.clone());
    access.spawn_udp_recvs(udp);
    access
        .spawn_tcp_forward(
            SocketAddr::from(([127, 0, 0, 1], env.http)),
            ControlStreamKind::Http,
        )
        .unwrap();
    access
        .spawn_tcp_forward(
            SocketAddr::from(([127, 0, 0, 1], env.sip_tcp)),
            ControlStreamKind::SipTcp,
        )
        .unwrap();
    rustpbx_master_ipc::set_runtime_handle(Handle::current());
    rustpbx_master::ipc_server::spawn_ipc_server(core.clone(), &env.sock, Handle::current())
        .unwrap();
    core
}

async fn wait_until(mut pred: impl AsyncFnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while !pred().await {
        if tokio::time::Instant::now() >= deadline {
            panic!("timeout waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Raw HTTP/1.1 GET through the master's forwarded fd; returns status line.
/// `/static/index.html` is served by the unconditional nest_service mount,
/// so it works regardless of the worker's cwd.
async fn http_get(port: u16) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET /api/config/phone HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).lines().next().unwrap_or("").to_string()
}

/// Raw REGISTER over UDP via the master; returns the first response line.
async fn sip_udp_register(port: u16, call_id: &str) -> String {
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let register = format!(
        "REGISTER sip:127.0.0.1 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{local};branch=z9hG4bK-e2e\r\n\
         From: <sip:sipbot@127.0.0.1>;tag=e2etag\r\n\
         To: <sip:sipbot@127.0.0.1>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 REGISTER\r\n\
         Max-Forwards: 70\r\n\
         Content-Length: 0\r\n\r\n",
        local = sock.local_addr().unwrap().port(),
    );
    sock.send_to(register.as_bytes(), ("127.0.0.1", port)).await.unwrap();
    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), sock.recv_from(&mut buf))
        .await
        .expect("no SIP response via UDP relay")
        .unwrap();
    String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_string()
}

/// Raw REGISTER over a TCP fd forwarded by the master; first response line.
async fn sip_tcp_register(port: u16, call_id: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let register = format!(
        "REGISTER sip:127.0.0.1 SIP/2.0\r\n\
         Via: SIP/2.0/TCP 127.0.0.1:5099;branch=z9hG4bK-tcp-e2e\r\n\
         From: <sip:sipbot@127.0.0.1>;tag=e2etag\r\n\
         To: <sip:sipbot@127.0.0.1>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 REGISTER\r\n\
         Max-Forwards: 70\r\n\
         Content-Length: 0\r\n\r\n"
    );
    s.write_all(register.as_bytes()).await.unwrap();
    let mut buf = [0u8; 2048];
    let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf))
        .await
        .expect("no SIP response via TCP fd forward")
        .unwrap();
    String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_worker_full_migration() {
    let Some(bin) = rustpbx_bin() else {
        eprintln!("skipping: target/debug/rustpbx not built (run `cargo build -p rustpbx --bin rustpbx`)");
        return;
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("debug"))
        .init();
    let env = setup_env();
    let core = boot_master(&env, &bin).await;

    // ── gen1 boot ──
    let (g1, child1) = core.upgrade_to().await.expect("real worker boot");
    spawn_reaper(core.clone(), g1, child1);

    // HTTP through master fd forwarding → worker router
    wait_until(
        || async { http_get(env.http).await.starts_with("HTTP/1.1") },
        "first HTTP response via fd forward",
    )
    .await;
    let status = http_get(env.http).await;
    assert!(status.contains("200"), "GET /static/index.html should be 200, got: {status}");

    // SIP UDP relay → auth 401 round-trip
    wait_until(
        || async { sip_udp_register(env.sip_udp, "e2e-udp-1@rustpbx").await.starts_with("SIP/2.0") },
        "first UDP REGISTER response",
    )
    .await;
    let resp = sip_udp_register(env.sip_udp, "e2e-udp-2@rustpbx").await;
    assert!(resp.contains("401"), "REGISTER should 401 (auth), got: {resp}");

    // SIP TCP fd forward
    let resp = sip_tcp_register(env.sip_tcp, "e2e-tcp-1@rustpbx").await;
    assert!(resp.contains("401"), "TCP REGISTER should 401, got: {resp}");

    // ── rotate to gen2 ──
    let (g2, child2) = core.upgrade_to().await.expect("rotation");
    spawn_reaper(core.clone(), g2, child2);
    assert_ne!(g1, g2);

    wait_until(
        || async { core.status().await["generations"].as_array().unwrap().len() == 1 },
        "gen1 exit after drain",
    )
    .await;
    let st = core.status().await;
    assert_eq!(st["generations"][0]["generation"], g2);

    // gen2 serves everything immediately after takeover
    let status = http_get(env.http).await;
    assert!(status.contains("200"), "HTTP after rotation, got: {status}");
    let resp = sip_udp_register(env.sip_udp, "e2e-udp-3@rustpbx").await;
    assert!(resp.contains("401"), "UDP REGISTER after rotation, got: {resp}");
    let resp = sip_tcp_register(env.sip_tcp, "e2e-tcp-2@rustpbx").await;
    assert!(resp.contains("401"), "TCP REGISTER after rotation, got: {resp}");
    assert_eq!(st["counters"]["upgrades_ok"], 2);
}
