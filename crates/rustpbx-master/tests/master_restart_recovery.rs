//! Master-restart recovery e2e: a REAL `rustpbx-master` daemon supervises a
//! REAL rustpbx worker. The master is SIGKILLed; a fresh master starts on
//! the same paths. The worker must SURVIVE (same pid), reconnect, and be
//! ADOPTED by the new master — proving crash recovery does not destroy
//! in-flight worker state.
//!
//! Requires: `target/debug/rustpbx` + `target/debug/rustpbx-master` built.

use rustpbx_master::admin::admin_sock_path;
use rustpbx_master::admin_client;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn rustpbx_bin() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/rustpbx");
    p.is_file().then(|| p.canonicalize().unwrap())
}

fn master_bin() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/rustpbx-master");
    p.is_file().then(|| p.canonicalize().unwrap())
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn master_restart_adopts_surviving_worker() {
    let Some(bin) = rustpbx_bin() else {
        eprintln!("skipping: rustpbx not built");
        return;
    };
    let Some(master) = master_bin() else {
        eprintln!("skipping: rustpbx-master not built");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("config.toml");
    let master_sock = dir.path().join("master.sock");
    let admin_sock = admin_sock_path(&master_sock, None);
    let releases = dir.path().join("releases");
    std::fs::create_dir_all(&releases).unwrap();

    let udp = free_udp_port();
    std::fs::write(
        &conf,
        format!(
            "[proxy]\naddr = \"127.0.0.1\"\nudp_port = {udp}\nrtp_start_port = 31000\nrtp_end_port = 31100\n"
        ),
    )
    .unwrap();

    // Stage r1 (real rustpbx binary as the release payload).
    let body = std::fs::read(&bin).unwrap();
    rustpbx_master::releases::stage(&releases, "r1", &body).unwrap();

    use std::process::Stdio;
    let logs = PathBuf::from("/tmp/master_restart_logs");
    let _ = std::fs::remove_dir_all(&logs);
    std::fs::create_dir_all(&logs).unwrap();
    let spawn_daemon = |tag: String| {
        let log = std::fs::File::create(logs.join(format!("{tag}.log"))).unwrap();
        tokio::process::Command::new(&master)
            .arg("--conf").arg(&conf)
            .arg("--master-sock").arg(&master_sock)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn master daemon")
    };

    // ── master #1: bootstraps r1, worker connects ──
    let mut daemon1 = spawn_daemon("m1".into());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut status = None;
    while tokio::time::Instant::now() < deadline {
        match admin_client::call(&admin_sock, "status", None).await {
            Ok(v) if v["generations"].as_array().map(|g| !g.is_empty()).unwrap_or(false) => {
                status = Some(v);
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(300)).await,
        }
    }
    let status = status.expect("master #1 never came up");
    let worker_pid = status["generations"][0]["pid"].as_i64().unwrap();
    assert_eq!(status["generations"][0]["state"], "active");

    // ── SIGKILL master #1 (worker survives — kill_on_drop dies with the process, not the worker) ──
    daemon1.kill().await.expect("kill master");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        Path::new(&format!("/proc/{worker_pid}")).exists(),
        "worker must SURVIVE the master SIGKILL (pid {worker_pid})"
    );

    // ── master #2: same paths; adoption grace lets the worker reconnect ──
    let mut daemon2 = spawn_daemon("m2".into());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut adopted = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(v) = admin_client::call(&admin_sock, "status", None).await {
            eprintln!("[poll] {}", v);
            if let Some(gens) = v["generations"].as_array() {
                if let Some(g) = gens.iter().find(|g| g["pid"].as_i64() == Some(worker_pid)) {
                    adopted = Some(g.clone());
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let g = adopted.expect("surviving worker was not adopted by the new master");
    assert_eq!(g["state"], "active", "adopted worker must be active: {g}");
    assert_eq!(g["release"], "r1", "adopted worker keeps its release: {g}");

    // The worker process itself never died (same pid still on the box).
    assert!(
        Path::new(&format!("/proc/{worker_pid}")).exists(),
        "worker pid {worker_pid} must have survived the master restart"
    );

    // Clean shutdown: tell the worker to stop via the admin-driven rotation
    // is overkill — kill both daemons (the worker is a child of daemon2 and
    // dies with its kill_on_drop).
    let _ = daemon2.start_kill();
    let _ = daemon1.start_kill();
}
