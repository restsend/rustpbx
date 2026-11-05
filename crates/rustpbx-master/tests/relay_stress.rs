//! Relay stress test (canary, `--ignored`): drives a call storm through a
//! cross-generation flip and asserts the relay keeps up without loss.
//!
//!  1. gen1 active; sipbot echo callee registers over SIP-over-TCP
//!  2. caller A establishes a long call on gen1 (pins gen1 draining)
//!  3. reload → gen2 active; gen1 draining with call A
//!  4. caller B drives `--total N --cps C` SHORT calls: every call is
//!     dispatched by gen2 to a callee whose TCP connection lives in gen1 —
//!     each INVITE/ACK/BYE traverses RelayToConn and every response
//!     traverses RelayToDialog
//!  5. asserts: the load process completes all calls; relay counters
//!     advanced with the call volume; gen1 stayed draining (no crash); no
//!     no-route growth; master/worker RSS bounded
//!
//! Timing contract: call A ends (BYE) BEFORE the callee's ~50s registration
//! refresh replaces its TCP connection — a refresh mid-call re-freezes the
//  agent-leg dialog target onto the new connection while the dialog lives
//! in gen1 (upstream dialog-freeze semantics; the relay cannot chase that).
//! The storm is sized to finish inside call A's window.
//!
//! Run: `cargo test -p rustpbx-master --test relay_stress -- --ignored`
//! Tunable via env: RELAY_STRESS_TOTAL (default 30), RELAY_STRESS_CPS (3),
//! RELAY_STRESS_HANGUP (6s). Requires target/debug/rustpbx + system sipbot.

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
    sip_tcp: u16,
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
    let sip_tcp = free_port();
    std::fs::write(
        &conf,
        format!(
            r#"
http_addr = "127.0.0.1:{http}"
log_level = "warn"
database_url = "sqlite://{db}?mode=rwc"
rtp_start_port = 27000
rtp_end_port = 28000

[proxy]
modules = ["acl", "auth", "registrar", "call"]
addr = "127.0.0.1"
udp_port = {sip_udp}
tcp_port = {sip_tcp}
registrar_expires = 300
realms = ["probe.rustpbx.com"]

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
            http = free_port(),
            db = db.display(),
            sip_udp = sip_udp,
            sip_tcp = sip_tcp,
            locator_db = locator_db.display(),
        ),
    )
    .unwrap();
    let sock = dir.path().join("master.sock");
    Env { _dir: dir, conf, sock, sip_udp, sip_tcp }
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
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

fn rss_kb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("VmRSS:").map(|v| {
                    v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
                })
            })
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "canary stress run — drive manually: cargo test -p rustpbx-master --test relay_stress -- --ignored"]
async fn relay_survives_call_storm_across_flip() {
    let total: u32 = std::env::var("RELAY_STRESS_TOTAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let cps: u32 = std::env::var("RELAY_STRESS_CPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let hangup: u32 = std::env::var("RELAY_STRESS_HANGUP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

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
    let g1_pid = core.status().await["generations"][0]["pid"].as_u64().unwrap_or(0);

    let logs = env._dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();

    // ── callee: serve mode over SIP-over-TCP (echo) — its connection lives
    // in gen1 for the whole run, so EVERY load call is a cross-gen dispatch ──
    let callee_conf = logs.join("callee.toml");
    std::fs::write(
        &callee_conf,
        format!(
            "addr = \"127.0.0.1:{local}\"\n\n[[accounts]]\nusername = \"echo\"\ndomain = \"probe.rustpbx.com\"\npassword = \"12345678\"\nproxy = \"127.0.0.1:{tcp};transport=tcp\"\nregister = true\n\n[accounts.answer]\naction = \"echo\"\n",
            local = free_udp_port(),
            tcp = env.sip_tcp,
        ),
    )
    .unwrap();
    let _callee = spawn_sipbot(
        &sipbot,
        &["serve".into(), "-C".into(), callee_conf.to_string_lossy().into_owned()],
        &logs.join("callee.log"),
    );
    wait_until(
        || async { core.status().await["conn_entries"].as_u64().unwrap_or(0) >= 1 },
        "callee TCP registration bridged",
    )
    .await;

    // ── caller A: long call pinning gen1 in draining ──
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
            "35".into(),
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
    let master_rss_before = rss_kb(std::process::id());

    // ── flip mid-call-A: gen2 active, gen1 draining with call A ──
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
    let gen2_pid = core.status().await["generations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["generation"] == g2)
        .and_then(|g| g["pid"].as_u64())
        .unwrap_or(0);
    let gen2_rss_before = rss_kb(gen2_pid.try_into().unwrap());

    // ── the storm: N short calls, every one a cross-gen dispatch ──
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
            hangup.to_string(),
            "--codecs".into(),
            "pcmu".into(),
            "--total".into(),
            total.to_string(),
            "--cps".into(),
            cps.to_string(),
        ],
        &logs.join("callerB.log"),
    );
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(60 + u64::from(total) * (u64::from(hangup) + 2));
    loop {
        match caller_b.try_wait() {
            Ok(Some(st)) => {
                assert!(st.success(), "load process failed (see callerB.log)");
                break;
            }
            Ok(None) => {}
            Err(e) => panic!("try_wait: {e}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "load process stalled beyond deadline"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let report = std::fs::read_to_string(logs.join("callerB.log")).unwrap_or_default();
    let done_line = report
        .lines()
        .rev()
        .find(|l| l.contains("Status:"))
        .unwrap_or("")
        .to_string();
    println!("=== load report ===\n{done_line}");

    // ── relay counters advanced with the volume ──
    let st = core.status().await;
    let to_conn = st["relay"]["to_conn_routed"].as_u64().unwrap_or(0);
    let to_dialog = st["relay"]["to_dialog_routed"].as_u64().unwrap_or(0);
    println!(
        "=== relay ===\nto_conn_routed={to_conn} to_dialog_routed={to_dialog} no_route={} / {} registry_entries={}",
        st["relay"]["to_conn_no_route"].as_u64().unwrap_or(0),
        st["relay"]["to_dialog_no_route"].as_u64().unwrap_or(0),
        st["registry_entries"].as_u64().unwrap_or(0)
    );
    assert!(
        to_conn >= u64::from(total) && to_dialog >= u64::from(total),
        "relay must have carried the storm (to_conn={to_conn} to_dialog={to_dialog} for {total} calls)"
    );
    assert_eq!(
        st["relay"]["to_conn_no_route"].as_u64().unwrap_or(0),
        0,
        "no cross-gen dispatch may be dropped (no_route)"
    );
    assert_eq!(
        st["relay"]["to_dialog_no_route"].as_u64().unwrap_or(0),
        0,
        "no relayed response may be dropped (no_route)"
    );

    // ── liveness & bounded memory ──
    let st = core.status().await;
    assert!(
        st["generations"].as_array().unwrap().len() == 2,
        "gen1 must still be draining with call A"
    );
    let gen2_rss_after = rss_kb(gen2_pid.try_into().unwrap());
    println!(
        "=== rss ===\nmaster: {}KB → {}KB | gen2: {}KB → {}KB",
        master_rss_before,
        rss_kb(std::process::id()),
        gen2_rss_before,
        gen2_rss_after
    );
    assert!(
        gen2_rss_after.saturating_sub(gen2_rss_before) < 200_000,
        "gen2 RSS growth beyond 200MB indicates a leak"
    );

    // ── caller A hangs up naturally (BYE at t=60s) → gen1 drains → exits.
    // (Killing the process would orphan call A's caller leg without a BYE —
    // the dialog would hang until RTP/session timers and never drain.) ──
    // Caller A ends naturally (BYE at its hangup timer). Note: gen1's exit
    // relies on call A's dialogs ending — the agent leg rides the callee's
    // TCP connection, which sipbot replaces every ~50s (registration refresh).
    // With call A sized at 35s the BYE lands before that churn.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let st = core.status().await;
        let gens = st["generations"].as_array().unwrap();
        if gens.len() == 1 && gens[0]["state"] == "active" {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "=== final state ===\n{}",
                serde_json::to_string_pretty(&st).unwrap_or_default()
            );
            panic!("timeout waiting for gen1 exits after drained; gen2 remains");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let st = core.status().await;
        if st["dialog_entries"].as_u64().unwrap_or(0) == 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "=== final state (dialog not empty) ===\n{}",
                serde_json::to_string_pretty(&st).unwrap_or_default()
            );
            panic!("timeout waiting for dialog table empty");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("=== stress passed ===");
}
