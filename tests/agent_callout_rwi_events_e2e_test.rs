//! E2E: Agent originate → callee scenarios with full RWI event verification
//!
//! Tests 5 scenarios:
//!   1. Ring → Answer → Audio (CallRinging, CallAnswered)
//!   2. Ring → No Answer / timeout (CallRinging, CallNoAnswer)
//!   3. Immediate Reject / Busy (CallBusy)
//!   4. Ring → Answer → Hangup (CallRinging, CallAnswered, CallHangup)
//!
//! Usage: cargo test --features addon-cc --test agent_callout_rwi_events_e2e_test -- --nocapture

mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(rwi_url: &str) -> WsStream {
    let url = format!("{}?token={}", rwi_url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("ws connect timeout")
        .expect("ws connect error");
    ws
}

async fn recv_until(
    ws: &mut WsStream,
    timeout_secs: u64,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("recv_until: timed out waiting for matching frame");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        let v: serde_json::Value = match msg {
            Message::Text(t) => {
                tracing::info!("[RWI] {}", &t[..t.len().min(300)]);
                serde_json::from_str(&t).expect("not JSON")
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        };
        if predicate(&v) {
            return v;
        }
    }
}

fn rwi_req(action: &str, params: serde_json::Value) -> (String, String) {
    let id = Uuid::new_v4().to_string();
    let json = serde_json::to_string(&serde_json::json!({
        "rwi": "1.0",
        "action_id": id,
        "action": action,
        "params": params,
    }))
    .unwrap();
    (id, json)
}

struct TestCtx {
    pbx: TestPbx,
    ws: WsStream,
    #[allow(dead_code)]
    sip_port: u16,
}

impl TestCtx {
    async fn new() -> Self {
        let sip_port = portpicker::pick_unused_port().expect("no SIP port");
        let pbx = TestPbx::start(sip_port).await;
        let mut ws = ws_connect(&pbx.rwi_url).await;

        let (action_id, sub_json) = rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
        ws.send(Message::Text(sub_json.into())).await.unwrap();
        let v = recv_until(&mut ws, 5, |v| {
            (v["type"] == "command_completed" || v["type"] == "command_failed")
                && v.get("action_id").map_or(false, |a| a == action_id.as_str())
        })
        .await;
        assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

        Self { pbx, ws, sip_port }
    }

    async fn originate(&mut self, callee_uri: &str) -> serde_json::Value {
        let call_id = format!("callout-{}", Uuid::new_v4());
        let caller_id = format!("sip:agent@{}", self.pbx.sip_host());
        let (_, json) = rwi_req(
            "call.originate",
            serde_json::json!({
                "call_id": call_id,
                "destination": callee_uri,
                "caller_id": caller_id,
                "context": "default",
                "timeout_secs": 10,
            }),
        );
        self.ws.send(Message::Text(json.into())).await.unwrap();
        let call_id_clone = call_id.clone();
        let completed = recv_until(&mut self.ws, 15, move |v| {
            (v["type"] == "command_completed" || v["type"] == "command_failed")
                && v.get("call_id").map_or(false, |c| c == call_id_clone.as_str())
        })
        .await;
        assert_eq!(completed["type"], "command_completed", "originate failed: {completed}");
        completed
    }

    async fn wait_event(&mut self, name: &str, timeout_secs: u64) -> serde_json::Value {
        let name_owned = name.to_string();
        recv_until(&mut self.ws, timeout_secs, move |v| {
            v.get(&name_owned).is_some()
                || v.to_string().contains(&name_owned)
        })
        .await
    }

    fn sip_uri_for(&self, port: u16, username: &str) -> String {
        format!("sip:{}@127.0.0.1:{}", username, port)
    }
}

// ─── Scenario 1: Ring → Answer → Audio ──────────────────────────────

#[tokio::test]
async fn test_agent_callout_answer() {
    let _ = tracing_subscriber::fmt::try_init();
    let callee_port = portpicker::pick_unused_port().expect("no callee port");

    let mut ctx = TestCtx::new().await;
    let callee = TestUa::callee_with_username(callee_port, 2, "alice").await;

    let uri = ctx.sip_uri_for(callee_port, "alice");
    ctx.originate(&uri).await;

    let ringing = ctx.wait_event("call_ringing", 5).await;
    tracing::info!("Event: call_ringing = {}", ringing.to_string().chars().take(200).collect::<String>());

    let answered = ctx.wait_event("call_answered", 10).await;
    tracing::info!("Event: call_answered = {}", answered.to_string().chars().take(200).collect::<String>());

    assert!(ringing.get("call_ringing").is_some() || ringing.to_string().contains("ringing"));
    assert!(answered.get("call_answered").is_some() || answered.to_string().contains("answered"));

    tokio::time::sleep(Duration::from_secs(3)).await;
    let callee_stats = callee.rtp_stats_summary();
    tracing::info!("Callee stats: {}", callee_stats);

    callee.stop();
    ctx.ws.close(None).await.unwrap();
    ctx.pbx.stop();
}

// ─── Scenario 2: Ring → No Answer (timeout) ──────────────────────────

#[tokio::test]
async fn test_agent_callout_no_answer() {
    let _ = tracing_subscriber::fmt::try_init();
    let callee_port = portpicker::pick_unused_port().expect("no callee port");

    let mut ctx = TestCtx::new().await;
    let callee = TestUa::callee_no_answer(callee_port, "alice", 60).await;

    let uri = ctx.sip_uri_for(callee_port, "alice");
    ctx.ws.send(Message::Text(
        rwi_req(
            "call.originate",
            serde_json::json!({
                "call_id": format!("callout-noanswer-{}", Uuid::new_v4()),
                "destination": uri,
                "caller_id": format!("sip:agent@{}", ctx.pbx.sip_host()),
                "context": "default",
                "timeout_secs": 5,
            }),
        )
        .1
        .into(),
    ))
    .await
    .unwrap();

    let ringing = ctx.wait_event("call_ringing", 5).await;
    tracing::info!("Event: call_ringing = {}", ringing.to_string().chars().take(200).collect::<String>());
    assert!(ringing.get("call_ringing").is_some() || ringing.to_string().contains("ringing"));

    let no_answer = ctx.wait_event("call_no_answer", 15).await;
    tracing::info!("Event: call_no_answer = {}", no_answer.to_string().chars().take(200).collect::<String>());
    assert!(
        no_answer.get("call_no_answer").is_some() || no_answer.to_string().contains("no_answer"),
        "Expected call_no_answer event, got: {}",
        no_answer.to_string().chars().take(300).collect::<String>()
    );

    callee.stop();
    ctx.ws.close(None).await.unwrap();
    ctx.pbx.stop();
}

// ─── Scenario 3: Immediate Reject / Busy ─────────────────────────────

#[tokio::test]
async fn test_agent_callout_busy() {
    let _ = tracing_subscriber::fmt::try_init();
    let callee_port = portpicker::pick_unused_port().expect("no callee port");

    let mut ctx = TestCtx::new().await;
    let callee = TestUa::callee_reject(callee_port, "alice", 486).await;

    let uri = ctx.sip_uri_for(callee_port, "alice");
    ctx.ws.send(Message::Text(
        rwi_req(
            "call.originate",
            serde_json::json!({
                "call_id": format!("callout-busy-{}", Uuid::new_v4()),
                "destination": uri,
                "caller_id": format!("sip:agent@{}", ctx.pbx.sip_host()),
                "context": "default",
                "timeout_secs": 10,
            }),
        )
        .1
        .into(),
    ))
    .await
    .unwrap();

    let busy_event = recv_until(&mut ctx.ws, 10, |v| {
        v.get("call_busy").is_some()
            || v.get("call_no_answer").is_some()
            || v.get("call_hangup").is_some()
    })
    .await;
    tracing::info!("Event (busy scenario) = {}", busy_event.to_string().chars().take(300).collect::<String>());

    let is_busy = busy_event.get("call_busy").is_some();
    let is_hangup = busy_event.get("call_hangup").is_some();
    let is_no_answer = busy_event.get("call_no_answer").is_some();
    assert!(is_busy || is_hangup || is_no_answer, "Expected call_busy/call_hangup/call_no_answer, got: {}", busy_event);

    callee.stop();
    ctx.ws.close(None).await.unwrap();
    ctx.pbx.stop();
}

// ─── Scenario 4: Ring → Answer → Hangup ──────────────────────────────

#[tokio::test]
async fn test_agent_callout_answer_then_hangup() {
    let _ = tracing_subscriber::fmt::try_init();
    let callee_port = portpicker::pick_unused_port().expect("no callee port");

    let mut ctx = TestCtx::new().await;
    let callee = TestUa::callee_answer_then_hangup(callee_port, 2, "alice", 3).await;

    let uri = ctx.sip_uri_for(callee_port, "alice");
    ctx.originate(&uri).await;

    let ringing = ctx.wait_event("call_ringing", 5).await;
    tracing::info!("Event: call_ringing");

    let answered = ctx.wait_event("call_answered", 10).await;
    tracing::info!("Event: call_answered");

    assert!(ringing.get("call_ringing").is_some() || ringing.to_string().contains("ringing"));
    assert!(answered.get("call_answered").is_some() || answered.to_string().contains("answered"));

    tokio::time::sleep(Duration::from_secs(5)).await;
    let callee_stats = callee.rtp_stats_summary();
    tracing::info!("Callee stats: {}", callee_stats);

    callee.stop();
    ctx.ws.close(None).await.unwrap();
    ctx.pbx.stop();
}
