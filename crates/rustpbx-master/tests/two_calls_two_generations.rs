//! Two concurrent calls on two different worker generations:
//!
//!  1. gen1 active; echo1 registered (serve #1)
//!  2. caller A → `sip:echo1@probe.rustpbx.com` → established ON GEN1
//!  3. reload → gen2 activates; gen1 drains but keeps call A alive
//!  4. caller B → `sip:echo2@probe.rustpbx.com` → established ON GEN2
//!     (a draining generation rejects new calls with 503 — B succeeding
//!     proves it landed on the new generation)
//!  5. both dialogs reported (`dialog_entries ≥ 2`), both generations alive
//!  6. callers hang up in order: B first (gen2 keeps serving), then A
//!     (gen1 finally drains out and exits)
//!
//! Requires: `target/debug/rustpbx` built + system `sipbot` on PATH.

use rustpbx_master::access::Access;
use rustpbx_master::config::ConfigSubset;
use rustpbx_master::supervisor::{spawn_reaper, SupCore};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;

fn rustpbx_bin() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/rustpbx");
    if p.is_file() { Some(p.canonicalize().unwrap()) } else { None }
}

fn sipbot_bin() -> Option<PathBuf> {
    let out = std::process::Command::new("which").arg("sipbot").output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() { None } else { Some(PathBuf::from(p)) }
}

struct Env {
    _dir: tempfile::TempDir,
    conf: PathBuf,
    sock: PathBuf,
    sip_udp: u16,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn stage(releases: &Path, name: &str, bin: &Path) {
    let body = std::fs::read(bin).unwrap();
    rustpbx_master::releases::stage(releases, name, &body).unwrap();
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
log_level = "info"
database_url = "sqlite://{}?mode=rwc"
rtp_start_port = 29000
rtp_end_port = 30000

[proxy]
modules = ["acl", "auth", "registrar", "call"]
addr = "127.0.0.1"
udp_port = {sip_udp}
tcp_port = {sip_tcp}
registrar_expires = 300
realms = ["probe.rustpbx.com"]

[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
id = 0
enabled = true
username = "sipbot"
password = "12345678"

[[proxy.user_backends.users]]
id = 1
enabled = true
username = "sipbot2"
password = "12345678"

[[proxy.user_backends.users]]
id = 2
enabled = true
username = "echo1"
password = "12345678"

[[proxy.user_backends.users]]
id = 3
enabled = true
username = "echo2"
password = "12345678"
"#,
            db.display(),
        ),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("releases")).unwrap();
    let sock = dir.path().join("master.sock");
    Env { _dir: dir, conf, sock, sip_udp }
}

async fn boot_master(env: &Env) -> Arc<SupCore> {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let bind_ip = conf.bind_ip().unwrap();
    let udp = Access::bind_udp(bind_ip, &[env.sip_udp]).unwrap();
    let udp_map: std::collections::HashMap<u16, Arc<tokio::net::UdpSocket>> =
        udp.iter().map(|(id, s)| (*id, s.clone())).collect();
    let core = SupCore::new(conf, &env.conf, &env.sock, udp_map).unwrap();
    let access = Access::new(core.clone());
    access.spawn_udp_recvs(udp);
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

fn spawn_sipbot(sipbot: &Path, args: &[String], log: &Path) -> tokio::process::Child {
    use std::process::Stdio;
    let log_file = std::fs::File::create(log).unwrap();
    tokio::process::Command::new(sipbot)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file))
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sipbot")
}

fn write_account_conf(path: &Path, username: &str, proxy: &str) {
    std::fs::write(
        path,
        format!(
            "[[accounts]]\nusername = \"{username}\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"{proxy}\"\nregister = true\n"
        ),
    )
    .unwrap();
}

/// Spawn a serve instance, retrying on free-port TOCTOU collisions (the
/// box has background services that auto-bind ephemeral UDP ports).
async fn spawn_serve_with_retry(
    sipbot: &Path,
    username: &str,
    proxy: &str,
    logs: &Path,
    tag: &str,
) -> (tokio::process::Child, u16) {
    for attempt in 0..5 {
        let port = free_udp_port();
        let conf = logs.join(format!("serve-{tag}-{attempt}.toml"));
        write_serve_conf(&conf, username, port, proxy, free_port());
        let mut child = spawn_sipbot(
            sipbot,
            &["serve".into(), "-C".into(), conf.to_string_lossy().into_owned()],
            &logs.join(format!("serve-{tag}-{attempt}.log")),
        );
        // The bind happens at startup: if it failed the process exits now.
        tokio::time::sleep(Duration::from_millis(700)).await;
        if child.try_wait().unwrap_or(None).is_none() {
            return (child, port);
        }
        eprintln!("serve {tag} attempt {attempt}: port {port} collided, retrying");
    }
    panic!("could not bind serve {tag} after 5 attempts");
}

fn write_serve_conf(path: &Path, username: &str, bind_port: u16, proxy: &str, http_port: u16) {
    // http_addr must be unique per instance: serve starts an answer-test UI
    // hardcoded to :8080 otherwise, so a second serve dies on bind.
    std::fs::write(
        path,
        format!(
            "addr = \"127.0.0.1:{bind_port}\"\nhttp_addr = \"127.0.0.1:{http_port}\"\n\n[[accounts]]\nusername = \"{username}\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"{proxy}\"\nregister = true\n\n[accounts.answer]\naction = \"echo\"\n"
        ),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_calls_live_on_two_generations() {
    let Some(bin) = rustpbx_bin() else {
        eprintln!("skipping: target/debug/rustpbx not built");
        return;
    };
    let Some(sipbot) = sipbot_bin() else {
        eprintln!("skipping: system sipbot not on PATH");
        return;
    };
    let env = setup_env();
    let core = boot_master(&env).await;
    let releases = env.conf.parent().unwrap().join("releases");
    stage(&releases, "r1", &bin);
    stage(&releases, "r2", &bin);

    let logs = PathBuf::from("/tmp/two_gen_logs");
    let _ = std::fs::remove_dir_all(&logs);
    std::fs::create_dir_all(&logs).unwrap();

    let proxy = format!("127.0.0.1:{}", env.sip_udp);

    // gen1 boots
    let (g1, child1) = core.upgrade_to("r1").await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);

    // Two independent echo callees.
    let (mut serve1, _) = spawn_serve_with_retry(&sipbot, "echo1", &proxy, &logs, "e1").await;
    let (mut serve2, _) = spawn_serve_with_retry(&sipbot, "echo2", &proxy, &logs, "e2").await;

    // caller A config (gen1 era) / caller B config (gen2 era)
    let conf_a = logs.join("callerA.toml");
    let conf_b = logs.join("callerB.toml");
    write_account_conf(&conf_a, "sipbot", &proxy);
    write_account_conf(&conf_b, "sipbot2", &proxy);

    // ── call A: established on gen1 ──
    let mut caller_a = spawn_sipbot(
        &sipbot,
        &[
            "call".into(),
            "-a".into(), format!("127.0.0.1:{}", free_udp_port()),
            "-C".into(), conf_a.to_string_lossy().into_owned(),
            "-t".into(), "sip:echo1@probe.rustpbx.com".into(),
            "--hangup".into(), "16".into(),
            "--codecs".into(), "pcmu".into(),
        ],
        &logs.join("callerA.log"),
    );
    wait_until(
        || async { core.status().await["dialog_entries"].as_u64().unwrap_or(0) >= 1 },
        "call A dialog reported",
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── rotate mid-call-A ──
    let (g2, child2) = core.upgrade_to("r2").await.expect("rotation");
    spawn_reaper(core.clone(), g2, child2);
    wait_until(
        || async {
            let st = core.status().await;
            let gens = st["generations"].as_array().unwrap();
            gens.len() == 2 && gens.iter().any(|g| g["generation"] == g2 && g["state"] == "active")
        },
        "gen2 active while gen1 draining",
    )
    .await;

    // ── call B: must land on gen2 (gen1 is draining = rejects new calls) ──
    let mut caller_b = spawn_sipbot(
        &sipbot,
        &[
            "call".into(),
            "-a".into(), format!("127.0.0.1:{}", free_udp_port()),
            "-C".into(), conf_b.to_string_lossy().into_owned(),
            "-t".into(), "sip:echo2@probe.rustpbx.com".into(),
            "--hangup".into(), "8".into(),
            "--codecs".into(), "pcmu".into(),
        ],
        &logs.join("callerB.log"),
    );
    wait_until(
        || async { core.status().await["dialog_entries"].as_u64().unwrap_or(0) >= 2 },
        "call B dialog reported (2 dialogs total)",
    )
    .await;

    // Both generations are alive simultaneously, each carrying one call.
    let st = core.status().await;
    assert_eq!(st["generations"].as_array().unwrap().len(), 2);

    // ── caller B finishes first: gen2 stays (it is the current generation) ──
    let status_b = caller_b.wait().await.expect("caller B wait");
    assert!(status_b.success(), "caller B must exit 0 (served by gen2); see callerB.log");
    // call A is still up: caller A running + at least its dialog reported.
    // (Avoid `== 1` here — A's cleanup can race the poll and land on 0.)
    let a_running = matches!(caller_a.try_wait(), Ok(None));
    assert!(a_running, "caller A must still be active while only B has ended");
    wait_until(
        || async { core.status().await["dialog_entries"].as_u64().unwrap_or(0) >= 1 },
        "call A dialog still reported after B ended",
    )
    .await;
    let st = core.status().await;
    assert_eq!(
        st["generations"].as_array().unwrap().len(),
        2,
        "gen1 must still be alive serving call A"
    );

    // ── caller A finishes: gen1 finally drains out and exits ──
    let status_a = caller_a.wait().await.expect("caller A wait");
    assert!(status_a.success(), "caller A must exit 0 (completed across rotation); see callerA.log");
    wait_until(
        || async { core.status().await["generations"].as_array().unwrap().len() == 1 },
        "gen1 exit after both calls done",
    )
    .await;
    let st = core.status().await;
    assert_eq!(st["generations"][0]["generation"], g2);
    assert_eq!(st["generations"][0]["state"], "active");
    assert_eq!(st["counters"]["worker_restarts"], 0);

    let _ = serve1.start_kill();
    let _ = serve2.start_kill();
}
