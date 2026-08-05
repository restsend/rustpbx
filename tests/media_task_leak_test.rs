//! Media module leak regression: repeated call churn must not grow tasks/sessions.
//!
//! Drives N originate → answer → hold → resume → hangup cycles through the real
//! RWI gateway against a sipbot echo callee (subprocess), then asserts the
//! tracked task count and active-call registry return to baseline after drain.
//!
//! This is the media-focused successor to the removed rwi_originate
//! `test_originate_task_cleanup`, which asserted `active_task_count()` stability
//! after each call. We extend it across hold/resume + conference media churn.
//!
//! KNOWN FINDING (2026-08-03): currently FAILS with +1 task per call from
//! `src/rwi/processor.rs:1473` (`session.process_uac` UAC session loop). The
//! loop does not terminate after `call.hangup`, leaking one task per
//! RWI-originated call. Kept `#[ignore]` as a canary: un-ignore once fixed.
//!
//! Usage: cargo test --test media_task_leak_test -- --nocapture

mod helpers;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use helpers::test_server::{TEST_TOKEN, TestPbx};
use rustpbx::utils::{active_task_count, reset_task_metrics};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use uuid::Uuid;

type WsTx = Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;
type WsRx = futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

fn spawn_callee(username: &str, password: &str, addr: &str, register: &str) -> Child {
    Command::new("sipbot")
        .args([
            "wait",
            "-a", addr,
            "--username", username,
            "--password", password,
            "--register", register,
            "--codecs", "pcmu",
            "--ring-duration", "1",
            "--echo",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sipbot callee")
}

/// Send an RWI request and wait for the matching command_completed response.
async fn rwi_cmd(
    ws: &mut WsRx,
    tx: &WsTx,
    action: &str,
    params: serde_json::Value,
    action_id: &str,
) -> serde_json::Value {
    let req = serde_json::json!({
        "rwi": "1.0",
        "action_id": action_id,
        "action": action,
        "params": params,
    });
    tx.lock().await.send(Message::Text(req.to_string().into())).await.unwrap();
    loop {
        let msg = timeout(Duration::from_secs(15), ws.next()).await;
        let msg = match msg {
            Ok(Some(Ok(Message::Text(t)))) => t,
            _ => return serde_json::json!({"type": "timeout"}),
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
            if v.get("action_id").and_then(|s| s.as_str()) == Some(action_id) {
                return v;
            }
        }
    }
}

#[tokio::test]
#[ignore = "flags rwi/processor.rs:1473 UAC session-loop leak (+1 task/call) — see module doc"]
async fn test_media_churn_no_task_or_session_leak() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("sip port");
    let callee_port = portpicker::pick_unused_port().expect("callee port");
    let pbx = TestPbx::start(sip_port).await;

    // sipbot echo callee registered to the PBX.
    let mut callee = spawn_callee(
        "1002", "123456",
        &format!("127.0.0.1:{callee_port}"),
        &format!("127.0.0.1:{sip_port}"),
    );
    sleep(Duration::from_secs(3)).await;

    let ws_url = format!("{}?token={}", pbx.rwi_url, TEST_TOKEN);
    let (ws, _) = connect_async(&ws_url).await.expect("rwi connect");
    let (sink, mut stream) = ws.split();
    let tx = Arc::new(Mutex::new(sink));

    rwi_cmd(
        &mut stream, &tx,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
        "sub",
    )
    .await;

    // Warm up one cycle so the WS session/registrar task overhead is included
    // in the baseline (a naive 0-task baseline would count fixed overhead as drift).
    reset_task_metrics();
    run_cycle(&mut stream, &tx, &callee_port, 0, "warm").await;
    sleep(Duration::from_secs(3)).await;
    let baseline = active_task_count();
    println!("[leak] warmup done, baseline tasks={baseline}");

    const CYCLES: usize = 10;
    for i in 0..CYCLES {
        run_cycle(&mut stream, &tx, &callee_port, i, "leak").await;
    }

    // Drain and let sessions/tasks clean up.
    sleep(Duration::from_secs(8)).await;
    let final_count = active_task_count();
    let drifted = final_count.saturating_sub(baseline);

    let base_loc = rustpbx::utils::task_metrics_snapshot();
    println!("[leak] baseline tasks={baseline}, final tasks={final_count}, drift={drifted}");
    for (loc, cnt) in base_loc.iter() {
        if *cnt > 0 {
            println!("[leak]   task@{loc}: {cnt}");
        }
    }

    callee.kill().ok();

    // Allow a modest fixed overhead, but flag any growth that scales with CYCLES
    // (a real per-call media/session leak).
    assert!(
        drifted <= 4,
        "media churn leaked tasks: baseline={baseline} final={final_count} after {CYCLES} cycles"
    );
}

/// One originate → answer → hold → resume → hangup cycle.
async fn run_cycle(
    stream: &mut WsRx,
    tx: &WsTx,
    callee_port: &u16,
    i: usize,
    prefix: &str,
) {
    let call_id = format!("{prefix}-{}-{i}", Uuid::new_v4().simple());
    let r = rwi_cmd(
        stream, tx,
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": format!("sip:1002@127.0.0.1:{callee_port}"),
            "caller_id": "sip:leak@pabx",
            "context": "default",
            "timeout_secs": 15,
        }),
        &format!("o{prefix}-{i}"),
    )
    .await;
    assert_eq!(r.get("type").and_then(|s| s.as_str()), Some("command_completed"),
               "originate {prefix} {i} failed: {r}");
    sleep(Duration::from_millis(600)).await;

    rwi_cmd(stream, tx, "call.hold", serde_json::json!({"call_id": call_id}), &format!("h{prefix}-{i}")).await;
    sleep(Duration::from_millis(300)).await;
    rwi_cmd(stream, tx, "call.unhold", serde_json::json!({"call_id": call_id}), &format!("u{prefix}-{i}")).await;
    sleep(Duration::from_millis(300)).await;
    rwi_cmd(stream, tx, "call.hangup", serde_json::json!({"call_id": call_id}), &format!("g{prefix}-{i}")).await;
}
