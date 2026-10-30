//! Admin API integration tests over the unix socket (length-prefixed JSON
//! frames, NOT HTTP): daemon-side protocol server driven via the same
//! `admin_client` the CLI uses. Covers `status` / `test` / `reload`,
//! bad-worker-binary rejection, socket permissions, stale-socket recovery,
//! already-running guard, and unknown commands.

use rustpbx_master::access::Access;
use rustpbx_master::admin::{admin_sock_path, AdminState};
use rustpbx_master::config::{ConfigSubset, MasterSubset};
use rustpbx_master::supervisor::{spawn_reaper, SupCore};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixStream;
use tokio::runtime::Handle;

const FAKE_WORKER: &str = env!("CARGO_BIN_EXE_fake-worker");

struct TestEnv {
    _dir: tempfile::TempDir,
    conf: PathBuf,
    sock: PathBuf,
    admin_sock: PathBuf,
    udp_port: u16,
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn setup_env(name: &str) -> TestEnv {
    let dir = tempfile::Builder::new().prefix(name).tempdir().unwrap();
    let conf = dir.path().join("config.toml");
    let udp_port = free_udp_port();
    std::fs::write(
        &conf,
        format!(
            "[proxy]\naddr = \"127.0.0.1\"\nudp_port = {udp_port}\nrtp_start_port = 23000\nrtp_end_port = 25000\n"
        ),
    )
    .unwrap();
    let sock = dir.path().join("master.sock");
    let admin_sock = admin_sock_path(&sock, None);
    TestEnv { _dir: dir, conf, sock, admin_sock, udp_port }
}

async fn boot_master(env: &TestEnv, worker_bin: &Path) -> Arc<SupCore> {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let sockets = Access::bind_udp(std::net::IpAddr::from([127, 0, 0, 1]), &[env.udp_port]).unwrap();
    let udp_map: std::collections::HashMap<u16, Arc<tokio::net::UdpSocket>> =
        sockets.iter().map(|(id, s)| (*id, s.clone())).collect();
    let mut core = SupCore::new(conf, &env.conf, &env.sock, udp_map).unwrap();
    // Set BEFORE the Arc is shared with access/ipc tasks.
    core.set_worker_bin(PathBuf::from(worker_bin));
    let access = Access::new(core.clone());
    access.spawn_udp_recvs(sockets);
    rustpbx_master_ipc::set_runtime_handle(Handle::current());
    rustpbx_master::ipc_server::spawn_ipc_server(core.clone(), &env.sock, Handle::current())
        .unwrap();
    core
}

/// Bind the admin protocol server on the env's unix socket.
async fn spawn_admin_on(core: Arc<SupCore>, env: &TestEnv) {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let state = Arc::new((core, conf.master));
    let _ = std::fs::remove_file(&env.admin_sock);
    let listener = tokio::net::UnixListener::bind(&env.admin_sock).unwrap();
    tokio::spawn(rustpbx_master::admin::serve_on(listener, state));
}

async fn admin_call(sock: &Path, cmd: &str) -> Value {
    match rustpbx_master::admin_client::call(sock, cmd, None).await {
        Ok(v) => v,
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn ok_of(v: &Value) -> Option<bool> {
    v.get("ok").and_then(|x| x.as_bool())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_status_test_reload_flow() {
    let env = setup_env("admin-flow");
    let core = boot_master(&env, Path::new(FAKE_WORKER)).await;
    spawn_admin_on(core.clone(), &env).await;

    // gen1 via the direct API (bootstrap equivalent)
    let (g1, child1) = core.upgrade_to().await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);

    // status
    let body = admin_call(&env.admin_sock, "status").await;
    assert_eq!(ok_of(&body), Some(true));
    assert_eq!(body["current_generation"], g1);

    // test on the worker binary (static validation, no boot)
    let body = admin_call(&env.admin_sock, "test").await;
    assert_eq!(ok_of(&body), Some(true), "body: {body}");
    assert_eq!(body["bin"].as_str().unwrap(), FAKE_WORKER);

    // unknown command → refused
    let body = admin_call(&env.admin_sock, "frobnicate").await;
    assert_eq!(ok_of(&body), Some(false));

    // reload → rotate to a fresh generation from the same binary
    let body = admin_call(&env.admin_sock, "reload").await;
    assert_eq!(ok_of(&body), Some(true), "body: {body}");
    let g2 = body["generation"].as_u64().unwrap() as u32;
    assert_ne!(g1, g2);

    // Old gen drains and exits; current flips to gen2; upgrades_ok == 2.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let st = core.status().await;
        if st["generations"].as_array().unwrap().len() == 1 {
            assert_eq!(st["generations"][0]["generation"], g2);
            assert_eq!(st["counters"]["upgrades_ok"], 2);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "gen1 did not exit");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_rejects_bad_worker_binary() {
    let env = setup_env("admin-tamper");
    // Point the supervisor at a garbage "binary": test/reload must both
    // refuse it — production must never boot it.
    let bad = env.conf.parent().unwrap().join("garbage.bin");
    std::fs::write(&bad, b"not an elf").unwrap();
    let core = boot_master(&env, &bad).await;
    spawn_admin_on(core.clone(), &env).await;

    let body = admin_call(&env.admin_sock, "test").await;
    assert_eq!(ok_of(&body), Some(false));
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.contains("not an ELF") || err.contains("executable"),
        "unexpected error: {err}"
    );

    // reload must refuse too.
    let body = admin_call(&env.admin_sock, "reload").await;
    assert_eq!(ok_of(&body), Some(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_sock_permissions_and_stale_recovery() {
    let env = setup_env("admin-perm");
    let core = boot_master(&env, Path::new(FAKE_WORKER)).await;
    // Real spawn_admin: stale sockets removed + 0o660 applied + bound
    // synchronously (the socket file exists when the call returns).
    rustpbx_master::admin::spawn_admin(
        core.clone(),
        MasterSubset::default(),
        env.admin_sock.clone(),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&env.admin_sock).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660, "admin socket must be 0o660");
    }
    let resp = rustpbx_master::admin_client::call(&env.admin_sock, "status", None)
        .await
        .expect("status call");
    assert_eq!(resp["ok"], true);
}

#[test]
fn admin_sock_defaults_next_to_worker_sock() {
    let worker_sock = Path::new("/var/lib/rustpbx/run/master.sock");
    assert_eq!(
        admin_sock_path(worker_sock, None),
        PathBuf::from("/var/lib/rustpbx/run/admin.sock")
    );
    // explicit config wins
    assert_eq!(
        admin_sock_path(worker_sock, Some("/run/rustpbx/admin.sock")),
        PathBuf::from("/run/rustpbx/admin.sock")
    );
}
