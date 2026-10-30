//! Rotation integration tests: in-process master + real fake-worker
//! subprocesses over the unix-socket IPC. Covers drain-then-rotate, crash
//! restart, UDP relay round-trip, and dialog-sticky routing.

use rustpbx_master::access::Access;
use rustpbx_master::config::ConfigSubset;
use rustpbx_master::supervisor::{spawn_reaper, SupCore};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;

const FAKE_WORKER: &str = env!("CARGO_BIN_EXE_fake-worker");

struct TestEnv {
    _dir: tempfile::TempDir,
    conf: PathBuf,
    sock: PathBuf,
    udp_port: u16,
}

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

fn stage_release(releases: &Path, name: &str, bin: &Path) {
    stage_release_with_args(releases, name, bin, &[]);
}

fn stage_release_with_args(releases: &Path, name: &str, bin: &Path, args: &[&str]) {
    let body = std::fs::read(bin).unwrap();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    // Stage through the real release pipeline (manifest + atomic rename);
    // the execve/ETXTBSY race is handled inside stage + spawn retry.
    rustpbx_master::releases::stage_with_args(releases, name, &body, &args).unwrap();
}

fn setup_env(name: &str) -> TestEnv {
    let dir = tempfile::Builder::new().prefix(name).tempdir().unwrap();
    let conf = dir.path().join("config.toml");
    let udp_port = free_udp_port();
    std::fs::write(
        &conf,
        format!(
            "[proxy]\naddr = \"127.0.0.1\"\nudp_port = {udp_port}\nrtp_start_port = 22000\nrtp_end_port = 24000\n"
        ),
    )
    .unwrap();
    let sock = dir.path().join("master.sock");
    std::fs::create_dir_all(dir.path().join("releases")).unwrap();
    TestEnv { _dir: dir, conf, sock, udp_port }
}

async fn boot_master(env: &TestEnv) -> Arc<SupCore> {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let sockets = Access::bind_udp(std::net::IpAddr::from([127, 0, 0, 1]), &[env.udp_port]).unwrap();
    let udp_map: HashMap<u16, Arc<tokio::net::UdpSocket>> =
        sockets.iter().map(|(id, s)| (*id, s.clone())).collect();
    let core = SupCore::new(conf, &env.conf, &env.sock, udp_map).unwrap();
    let access = Access::new(core.clone());
    access.spawn_udp_recvs(sockets);
    rustpbx_master_ipc::set_runtime_handle(Handle::current());
    rustpbx_master::ipc_server::spawn_ipc_server(core.clone(), &env.sock, Handle::current())
        .unwrap();
    core
}

async fn wait_until(mut pred: impl AsyncFnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !pred().await {
        if tokio::time::Instant::now() >= deadline {
            panic!("timeout waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_then_rotate() {
    let env = setup_env("drain-rotate");
    let core = boot_master(&env).await;
    let releases = env.conf.parent().unwrap().join("releases");
    stage_release(&releases, "r1", Path::new(FAKE_WORKER));
    stage_release(&releases, "r2", Path::new(FAKE_WORKER));

    // gen1 boot
    let (g1, child1) = core.upgrade_to("r1").await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);
    let st = core.status().await;
    assert_eq!(st["current_generation"], g1);
    assert_eq!(st["generations"][0]["state"], "active");
    assert_eq!(st["generations"][0]["accepting"], true);
    let rtp1: u16 = st["generations"][0]["rtp_slice"][0].as_u64().unwrap() as u16;

    // gen2 rotate: gen1 must drain and exit
    let (g2, child2) = core.upgrade_to("r2").await.expect("gen2 rotate");
    spawn_reaper(core.clone(), g2, child2);
    assert_ne!(g1, g2);
    let st = core.status().await;
    assert_eq!(st["current_generation"], g2);
    let rtp2: u16 = st["generations"].as_array().unwrap().iter()
        .find(|g| g["generation"] == g2).unwrap()["rtp_slice"][0]
        .as_u64().unwrap() as u16;
    assert_ne!(rtp1, rtp2, "generations must get disjoint RTP slices");

    wait_until(
        || async { core.status().await["generations"].as_array().unwrap().len() == 1 },
        "gen1 exit after drain",
    )
    .await;
    let st = core.status().await;
    assert_eq!(st["generations"][0]["generation"], g2);
    assert_eq!(st["counters"]["upgrades_ok"], 2);
    assert_eq!(st["counters"]["worker_restarts"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_restarts_worker() {
    let env = setup_env("crash-restart");
    let core = boot_master(&env).await;
    let releases = env.conf.parent().unwrap().join("releases");
    stage_release(&releases, "r1", Path::new(FAKE_WORKER));

    let (g1, child1) = core.upgrade_to("r1").await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);

    // SIGKILL the worker: supervisor must notice and boot a replacement.
    let pid: i32 = core.status().await["generations"][0]["pid"].as_i64().unwrap() as i32;
    unsafe { kill(pid) };

    wait_until(
        || async { core.status().await["current_generation"] != g1 && core.status().await["current_generation"] != 0 },
        "restart after crash",
    )
    .await;
    let st = core.status().await;
    assert_eq!(st["counters"]["worker_restarts"], 1);

    // The restarted worker must actually be ALIVE and serving — a dropped
    // kill_on_drop child used to die instantly here (vacuous pass before).
    let g2 = st["current_generation"].as_u64().unwrap() as u32;
    wait_until(
        || async {
            let st = core.status().await;
            st["generations"][0]["generation"] == g2
                && st["generations"][0]["state"] == "active"
                && st["generations"][0]["accepting"] == true
        },
        "restarted generation active & accepting",
    )
    .await;

    // …and it still relays UDP (PING → PONG through the new generation).
    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let public: SocketAddr = format!("127.0.0.1:{}", env.udp_port).parse().unwrap();
    client.send_to(b"PING", public).await.unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(10), client.recv_from(&mut buf))
        .await
        .expect("restarted worker must answer PING")
        .unwrap();
    assert_eq!(&buf[..n], b"PONG");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_relay_roundtrip() {
    let env = setup_env("udp-relay");
    let core = boot_master(&env).await;
    let releases = env.conf.parent().unwrap().join("releases");
    stage_release(&releases, "r1", Path::new(FAKE_WORKER));

    let (_g, child) = core.upgrade_to("r1").await.expect("gen1 boot");
    spawn_reaper(core.clone(), _g, child);

    // Client PING via the master's public port → fake worker PONG back.
    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let public: SocketAddr = format!("127.0.0.1:{}", env.udp_port).parse().unwrap();
    client.send_to(b"PING", public).await.unwrap();
    let mut buf = [0u8; 64];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let got = loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            panic!("no PONG from relayed datagram");
        }
        match tokio::time::timeout(remain, client.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => break &buf[..n],
            Ok(Err(e)) => panic!("recv: {e}"),
            Err(_) => panic!("timeout receiving PONG"),
        }
    };
    assert_eq!(got, b"PONG");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dialog_routing_sticky() {
    let env = setup_env("dialog-sticky");
    let core = boot_master(&env).await;
    let releases = env.conf.parent().unwrap().join("releases");
    // r1 drains slowly so the drain window is observable.
    stage_release_with_args(&releases, "r1", Path::new(FAKE_WORKER), &["--drain-secs", "3"]);
    stage_release(&releases, "r2", Path::new(FAKE_WORKER));

    // No worker yet: unroutable.
    assert_eq!(core.route_gen(Some("cid-x")).await, None);
    assert_eq!(core.route_gen(None).await, None);

    let (g1, child1) = core.upgrade_to("r1").await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);

    // Unknown Call-ID → current gen; known Call-ID → sticky owner.
    assert_eq!(core.route_gen(None).await, Some(g1));
    core.on_dialog_up(g1, "cid-a".into()).await;
    assert_eq!(core.route_gen(Some("cid-a")).await, Some(g1));

    // Rotate: gen1 drains (slowly) but keeps ownership of its dialogs.
    let (g2, child2) = core.upgrade_to("r2").await.expect("gen2");
    spawn_reaper(core.clone(), g2, child2);
    assert_eq!(core.route_gen(Some("cid-a")).await, Some(g1));
    assert_eq!(core.route_gen(Some("cid-b")).await, Some(g2));
    assert_eq!(core.route_gen(None).await, Some(g2));
    let g1_link = core.gen_link(g1).await.expect("gen1 link during drain");
    assert!(!g1_link.is_accepting(), "draining gen must not accept");

    // gen1 exits after its 3s drain → link gone and dialogs purged.
    wait_until(
        || async { core.gen_link(g1).await.is_none() && core.route_gen(Some("cid-a")).await == Some(g2) },
        "purge of drained gen dialogs",
    )
    .await;
}

// nix kill helper (dev-dependency-free via libc-free path)
unsafe fn kill(pid: i32) {
    unsafe { nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL).unwrap() };
}
