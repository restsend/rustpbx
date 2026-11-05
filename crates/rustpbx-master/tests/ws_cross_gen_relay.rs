//! Cross-generation SIP relay e2e (WS): same scenario as
//! `tcp_cross_gen_relay.rs`, but the callee registers over SIP-over-WebSocket
//! (sipbot `--ws-url`, WebRTC media) riding the fd-forwarded HTTP port.
//!
//! Verifies the WS-specific relay seams end-to-end: axum ws upgrade inside a
//! draining-capable worker, ConnUp(Wss/Ws) + WsChannel injection, text-frame
//! relay scan, and the virtual relay connection keyed by the ws peer addr.
//!
//! Requires: `target/debug/rustpbx` built + system `sipbot` on PATH (0.2.80+).

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
    let out = std::process::Command::new("which").arg("sipbot").output().ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() { None } else { Some(PathBuf::from(p)) }
}

struct Env {
    _dir: tempfile::TempDir,
    conf: PathBuf,
    sock: PathBuf,
    sip_udp: u16,
    http: u16,
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
    let db = dir.path().join("rustpbx.sqlite3");
    let locator_db = dir.path().join("locator.sqlite3");
    let conf = dir.path().join("conf.toml");
    let sip_udp = free_udp_port();
    let http = free_port();
    std::fs::write(
        &conf,
        format!(
            r#"
http_addr = "127.0.0.1:{http}"
log_level = "debug"
database_url = "sqlite://{db}?mode=rwc"
rtp_start_port = 27000
rtp_end_port = 28000

[proxy]
modules = ["acl", "auth", "registrar", "call"]
addr = "127.0.0.1"
udp_port = {sip_udp}
registrar_expires = 300
realms = ["probe.rustpbx.com"]
ws_handler = "/ws"

[proxy.locator]
type = "database"
url = "sqlite://{locator_db}?mode=rwc"

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
            http = http,
            db = db.display(),
            sip_udp = sip_udp,
            locator_db = locator_db.display(),
        ),
    )
    .unwrap();
    let sock = dir.path().join("master.sock");
    Env { _dir: dir, conf, sock, sip_udp, http }
}

async fn boot_master(env: &Env, worker_bin: &Path) -> Arc<SupCore> {
    let conf = ConfigSubset::load(&env.conf).unwrap();
    let bind_ip = conf.bind_ip().unwrap();
    let udp = Access::bind_udp(bind_ip, &[env.sip_udp]).unwrap();
    let udp_map: std::collections::HashMap<u16, Arc<tokio::net::UdpSocket>> =
        udp.iter().map(|(id, s)| (*id, s.clone())).collect();
    let mut core = SupCore::new(conf, &env.conf, &env.sock, udp_map).unwrap();
    core.set_worker_bin(PathBuf::from(worker_bin));
    let access = Access::new(core.clone());
    access.spawn_udp_recvs(udp);
    // HTTP fd forwarding: ws SIP upgrades (and console API) ride this port.
    access
        .spawn_tcp_forward(
            SocketAddr::from(([127, 0, 0, 1], env.http)),
            ControlStreamKind::Http,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_dispatched_leg_completes_across_generations() {
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

    // ── gen1 boots ──
    let (g1, child1) = core.upgrade_to().await.expect("gen1 boot");
    spawn_reaper(core.clone(), g1, child1);

    let logs = env._dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();

    // ── callee: serve mode over SIP-over-WebSocket (WebRTC media), echo ──
    let callee_conf = logs.join("callee.toml");
    std::fs::write(
        &callee_conf,
        format!(
            "addr = \"127.0.0.1:{local}\"\n\n[[accounts]]\nusername = \"echo\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"127.0.0.1:{udp}\"\ntransport = \"ws\"\ntransport_ws_url = \"ws://127.0.0.1:{http}/ws\"\nregister = true\n\n[accounts.answer]\naction = \"echo\"\n",
            local = free_udp_port(),
            udp = env.sip_udp,
            http = env.http,
        ),
    )
    .unwrap();
    let _callee = spawn_sipbot(
        &sipbot,
        &[
            "serve".into(),
            "--ws-url".into(),
            format!("ws://127.0.0.1:{}/ws", env.http),
            "-C".into(),
            callee_conf.to_string_lossy().into_owned(),
        ],
        &logs.join("callee.log"),
    );
    // Registration must land AND be bridged: ConnTable entry on gen1.
    wait_until(
        || async { core.status().await["conn_entries"].as_u64().unwrap_or(0) >= 1 },
        "callee WS registration bridged (conn_entries ≥ 1)",
    )
    .await;

    // ── caller A (UDP): long call that pins gen1 in draining ──
    let caller_conf = logs.join("caller.toml");
    std::fs::write(
        &caller_conf,
        format!(
            "[[accounts]]\nusername = \"sipbot\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"127.0.0.1:{udp}\"\nregister = true\n",
            udp = env.sip_udp,
        ),
    )
    .unwrap();
    let mut caller_a = spawn_sipbot(
        &sipbot,
        &[
            "call".into(),
            "-a".into(),
            format!("127.0.0.1:{}", free_udp_port()),
            "-C".into(),
            caller_conf.to_string_lossy().into_owned(),
            "-t".into(),
            "sip:echo@probe.rustpbx.com".into(),
            "--hangup".into(),
            "45".into(),
            "--codecs".into(),
            "pcmu".into(),
        ],
        &logs.join("callerA.log"),
    );
    wait_until(
        || async { core.status().await["dialog_entries"].as_u64().unwrap_or(0) >= 4 },
        "call A established on gen1",
    )
    .await;

    // ── flip: gen2 activates; gen1 stays draining with call A ──
    let (g2, child2) = core.upgrade_to().await.expect("mid-call rotation");
    spawn_reaper(core.clone(), g2, child2);
    wait_until(
        || async {
            let st = core.status().await;
            let gens = st["generations"].as_array().unwrap();
            gens.len() == 2
                && gens.iter().any(|g| g["state"] == "draining")
                && gens.iter().any(|g| g["state"] == "active")
        },
        "gen2 active while gen1 draining",
    )
    .await;

    // ── caller B (UDP): dispatched by gen2 to a callee whose WS connection
    // lives in gen1 — the cross-gen relay's exact production scenario ──
    let mut caller_b = spawn_sipbot(
        &sipbot,
        &[
            "call".into(),
            "-a".into(),
            format!("127.0.0.1:{}", free_udp_port()),
            "-C".into(),
            caller_conf.to_string_lossy().into_owned(),
            "-t".into(),
            "sip:echo@probe.rustpbx.com".into(),
            "--hangup".into(),
            "10".into(),
            "--codecs".into(),
            "pcmu".into(),
        ],
        &logs.join("callerB.log"),
    );
    let mut b_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        match caller_b.try_wait() {
            Ok(Some(st)) => {
                assert!(st.success(), "caller B must complete (see callerB.log)");
                b_done = true;
                break;
            }
            Ok(None) => {}
            Err(e) => panic!("try_wait: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(b_done, "caller B never completed within 60s");
    let st = core.status().await;
    let to_conn = st["relay"]["to_conn_routed"].as_u64().unwrap_or(0);
    let to_dialog = st["relay"]["to_dialog_routed"].as_u64().unwrap_or(0);
    assert!(to_conn >= 1, "agent-leg INVITE must be relayed (to_conn_routed)");
    assert!(to_dialog >= 1, "callee responses must be relayed back (to_dialog_routed)");

    // ── caller A ends naturally → gen1 drains → exits; gen2 remains ──
    // (caller A's exit code is not an assertion point: its agent leg may be
    // cut when the callee's WS churn replaces the flow — upstream semantics.)
    let a_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        match caller_a.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => panic!("try_wait: {e}"),
        }
        assert!(
            tokio::time::Instant::now() < a_deadline,
            "caller A never exited (see callerA.log)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    wait_until(
        || async {
            let st = core.status().await;
            let gens = st["generations"].as_array().unwrap();
            gens.len() == 1 && gens[0]["state"] == "active"
        },
        "gen1 exits after drained; gen2 remains",
    )
    .await;
    wait_until(
        || async { core.status().await["dialog_entries"].as_u64().unwrap_or(0) == 0 },
        "dialog table empty after all calls ended",
    )
    .await;
}
