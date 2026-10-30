//! Active-call rotation e2e: a REAL rustpbx worker serves a REAL sipbot
//! call (`sips:echo@probe.rustpbx.com`, answered by a sipbot echo callee).
//! Mid-call we rotate to a new generation and assert the call survives:
//!
//!  1. gen1 active → caller INVITE → dialog reported (`dialog_entries ≥ 1`)
//!  2. reload → gen2 boots & activates; gen1 stays up (draining) with the
//!     live dialog — two generations coexist while the call runs
//!  3. caller hangs up after ~14s of media (RTP echoed by the callee)
//!  4. gen1 reports Drained → exits; gen2 is the only generation left
//!  5. caller exit code 0 = full call completed across the rotation
//!
//! Requires: `target/debug/rustpbx` built + system `sipbot` on PATH.

use rustpbx_master::access::{Access, ControlStreamKind};
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
    // System PATH sipbot (per project rule: never build our own).
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
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
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
log_level = "debug"
database_url = "sqlite://{}?mode=rwc"
rtp_start_port = 27000
rtp_end_port = 28000

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
username = "echo"
password = "12345678"
"#,
            db.display(),
        ),
    )
    .unwrap();
    let sock = dir.path().join("master.sock");
    Env { _dir: dir, conf, sock, sip_udp }
}

async fn boot_master(env: &Env, worker_bin: &Path) -> Arc<SupCore> {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let bind_ip = conf.bind_ip().unwrap();
    let udp = Access::bind_udp(bind_ip, &[env.sip_udp]).unwrap();
    let udp_map: std::collections::HashMap<u16, Arc<tokio::net::UdpSocket>> =
        udp.iter().map(|(id, s)| (*id, s.clone())).collect();
    let mut core = SupCore::new(conf, &env.conf, &env.sock, udp_map).unwrap();
    // Set BEFORE the Arc is shared with access/ipc tasks.
    core.set_worker_bin(PathBuf::from(worker_bin));
    let access = Access::new(core.clone());
    access.spawn_udp_recvs(udp);
    access
        .spawn_tcp_forward(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            ControlStreamKind::Http,
        )
        .ok(); // HTTP not needed for this test; ignore bind result on :0
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

/// Spawn a long-running sipbot process with logs captured into the tempdir.
/// Fixed location so failed-run logs survive the test's tempdir cleanup.
fn pathlib_debug_dir() -> PathBuf {
    PathBuf::from("/tmp/active_call_logs")
}

fn spawn_sipbot(
    sipbot: &Path,
    args: &[String],
    log: &Path,
) -> tokio::process::Child {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_call_survives_rotation() {
    let Some(bin) = rustpbx_bin() else {
        eprintln!("skipping: target/debug/rustpbx not built");
        return;
    };
    let Some(sipbot) = sipbot_bin() else {
        eprintln!("skipping: system sipbot not on PATH");
        return;
    };
    let env = setup_env();
    let core = boot_master(&env, &bin).await;

    // ── gen1 boots (real rustpbx worker) ──
    let (g1, child1) = core.upgrade_to().await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);

    let proxy = format!("127.0.0.1:{}", env.sip_udp);
    let logs = pathlib_debug_dir();
    let _ = std::fs::remove_dir_all(&logs);
    std::fs::create_dir_all(&logs).unwrap();

    // ── callee: serve mode registers `echo@probe.rustpbx.com` and answers
    // with RTP echo (wait-mode cannot set the AOR domain: its -d flag is
    // dropped before the account is built) ──
    let callee_port = free_udp_port();
    let callee_conf = logs.join("callee.toml");
    std::fs::write(
        &callee_conf,
        format!(
            "addr = \"127.0.0.1:{callee_port}\"\n\n[[accounts]]\nusername = \"echo\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"{proxy}\"\nregister = true\n\n[accounts.answer]\naction = \"echo\"\n"
        ),
    )
    .unwrap();
    let mut callee = spawn_sipbot(
        &sipbot,
        &[
            "serve".into(),
            "-C".into(), callee_conf.to_string_lossy().into_owned(),
        ],
        &logs.join("callee.log"),
    );

    // ── determinism gate: do not dial until the callee's registration has
    // actually landed (under load the INVITE can otherwise beat it → 480) ──
    {
        let callee_log = logs.join("callee.log");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let registered = std::fs::read_to_string(&callee_log)
                .map(|c| c.contains("Registered successfully"))
                .unwrap_or(false);
            if registered {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "callee never registered"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // ── caller: registers as sipbot@probe.rustpbx.com (config file needed:
    // the `call` subcommand has no -d flag, and the INVITE 407 challenge
    // uses the R-URI domain as digest realm) ──
    let caller_conf = logs.join("caller.toml");
    std::fs::write(
        &caller_conf,
        format!(
            "[[accounts]]\nusername = \"sipbot\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"{proxy}\"\nregister = true\n"
        ),
    )
    .unwrap();
    // Under heavy machine load the callee's registration can lag the
    // caller's INVITE (→ 480). Redial until a dialog is actually reported.
    let caller_args = || {
        vec![
            "call".into(),
            "-a".into(), format!("127.0.0.1:{}", free_udp_port()),
            "-C".into(), caller_conf.to_string_lossy().into_owned(),
            "-t".into(), "sip:echo@probe.rustpbx.com".into(),
            "--hangup".into(), "14".into(),
            "--codecs".into(), "pcmu".into(),
        ]
    };
    let mut caller: Option<tokio::process::Child> = None;
    let mut established = false;
    for attempt in 0..3u32 {
        let mut c = spawn_sipbot(&sipbot, &caller_args(), &logs.join(format!("caller{attempt}.log")));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            if core.status().await["dialog_entries"].as_u64().unwrap_or(0) >= 1 {
                established = true;
                break;
            }
            match c.try_wait() {
                Ok(Some(st)) => {
                    if st.success() {
                        // Completed without an observed poll window (poll race):
                        // the call DID complete — treat as established.
                        established = true;
                    }
                    eprintln!("call attempt {attempt}: exited ({st}); dialog seen={established}");
                    break;
                }
                Ok(None) => {}
                Err(e) => panic!("try_wait: {e}"),
            }
            assert!(tokio::time::Instant::now() < deadline, "call attempt {attempt} stalled");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if established {
            caller = Some(c);
            break;
        }
    }
    let mut caller = caller.expect("call never established after 3 attempts");
    // give ACK a moment: call is now established with media flowing.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── rotate MID-CALL ──
    let (g2, child2) = core.upgrade_to().await.expect("mid-call rotation");
    spawn_reaper(core.clone(), g2, child2);
    assert_ne!(g1, g2);

    // gen1 must STILL be alive (draining with the active dialog) alongside
    // the new active gen2 — two real workers coexist during the call.
    wait_until(
        || async {
            let st = core.status().await;
            let gens = st["generations"].as_array().unwrap();
            gens.len() == 2
                && gens.iter().any(|g| g["generation"] == g2 && g["state"] == "active")
        },
        "gen2 active while gen1 still draining",
    )
    .await;

    // ── caller hangs up at ~14s → BYE → gen1 drains → exits ──
    let caller_status = caller.wait().await.expect("caller wait");
    assert!(
        caller_status.success(),
        "sipbot caller must exit 0 (call completed across rotation); see caller.log"
    );

    // gen1 exits after the dialog went away; gen2 is all that remains.
    wait_until(
        || async { core.status().await["generations"].as_array().unwrap().len() == 1 },
        "gen1 exit after drained",
    )
    .await;
    let st = core.status().await;
    assert_eq!(st["generations"][0]["generation"], g2);
    assert_eq!(st["generations"][0]["state"], "active");
    assert_eq!(st["counters"]["upgrades_ok"], 2);
    assert_eq!(st["counters"]["worker_restarts"], 0, "drain-exit is not a crash");

    // callee gets the BYE / its own hangup timer — stop it if still running.
    let _ = callee.start_kill();

    // The dialog table is empty again (BYE purged via DialogDown).
    wait_until(
        || async { core.status().await["dialog_entries"].as_u64().unwrap_or(0) == 0 },
        "dialog table empty after hangup",
    )
    .await;
}
