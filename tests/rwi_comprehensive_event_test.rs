//! E2E: Comprehensive RWI Event Verification
//!
//! Tests core RWI event types through the real RWI gateway and WebSocket.
//! Uses a background reader to capture ALL events without dropping any.
//!
//! Event categories covered:
//!   — Call lifecycle:  call_incoming, call_ringing, call_answered, call_hangup
//!   — Recording:        record_started, record_stopped
//!   — Queue:            queue_joined, queue_left
//!   — IVR:              ivr_step_trace, ivr_node_entered, ivr_node_exited, ivr_flow_completed
//!   — Media:            media_hold_started, media_hold_stopped, media_play_started, media_play_finished
//!   — Conference:       conference_created, call_bridged
//!   — DTMF:             dtmf
//!
//! Usage: cargo test --test rwi_comprehensive_event_test -- --nocapture

mod helpers;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use helpers::test_server::{TEST_TOKEN, TestPbx};
use rustpbx::config::{MediaProxyMode, ProxyConfig, RecordingPolicy};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use uuid::Uuid;

type WsRx = futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;
type WsTx = Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;

struct EventLog {
    events: Mutex<Vec<serde_json::Value>>,
}

impl EventLog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }
    async fn push(&self, v: serde_json::Value) {
        self.events.lock().await.push(v);
    }
    async fn snapshot(&self) -> Vec<serde_json::Value> {
        self.events.lock().await.clone()
    }
    async fn count(&self, event_type: &str) -> usize {
        self.events
            .lock()
            .await
            .iter()
            .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some(event_type))
            .count()
    }
    async fn has_action_id(&self, action_id: &str) -> bool {
        self.events
            .lock()
            .await
            .iter()
            .any(|v| v.get("action_id").and_then(|s| s.as_str()) == Some(action_id))
    }
}

async fn event_type_name(v: &serde_json::Value) -> Option<String> {
    v.get("event_type")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

fn start_bg_reader(mut ws_rx: WsRx, log: Arc<EventLog>) -> tokio::sync::oneshot::Sender<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut stop_rx = std::pin::pin!(stop_rx);
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                msg = ws_rx.next() => {
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                                let et = v.get("event_type").and_then(|s| s.as_str()).unwrap_or("?");
                                let preview: String = t.chars().take(150).collect();
                                eprintln!("  [WS] event_type={et} | {preview}");
                                log.push(v).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Err(e)) => {
                            eprintln!("  [WS] error: {e}");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    stop_tx
}

struct TestCtx {
    pbx: TestPbx,
    _stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    _ws_tx: WsTx,
    log: Arc<EventLog>,
}

impl TestCtx {
    async fn new() -> Self {
        let sip_port = portpicker::pick_unused_port().unwrap();
        let pbx = TestPbx::start_with_inject(
            sip_port,
            helpers::test_server::TestPbxInject {
                proxy_config: Some(ProxyConfig {
                    media_proxy: MediaProxyMode::All,
                    recording: Some(RecordingPolicy {
                        enabled: Some(true),
                        auto_start: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await;

        let log = EventLog::new();
        let log2 = log.clone();

        // Connect WS
        let rwi_url = format!("{}?token={}", &pbx.rwi_url, TEST_TOKEN);
        let (ws, _) = timeout(Duration::from_secs(5), connect_async(&rwi_url))
            .await
            .expect("ws connect")
            .expect("ws connect error");

        let (ws_tx, ws_rx) = ws.split();
        let ws_tx = Arc::new(Mutex::new(ws_tx));

        // Subscribe
        let sub_id = Uuid::new_v4().to_string();
        {
            let sub_json = serde_json::to_string(&serde_json::json!({
                "rwi": "1.0", "action_id": sub_id,
                "action": "session.subscribe",
                "params": {"contexts": ["default"]},
            }))
            .unwrap();
            ws_tx
                .lock()
                .await
                .send(Message::Text(sub_json.into()))
                .await
                .unwrap();
        }

        // Spawn bg reader, then wait for sub response in the log
        let bg_log = log2.clone();
        let stop_tx = start_bg_reader(ws_rx, log2);

        let dl = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if bg_log.has_action_id(&sub_id).await {
                break;
            }
            if dl.elapsed() > Duration::from_secs(5) {
                panic!("subscribe timeout");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Self {
            pbx,
            _stop_tx: Some(stop_tx),
            _ws_tx: ws_tx,
            log,
        }
    }

    fn gw(&self) -> parking_lot::RwLockReadGuard<'_, rustpbx::rwi::RwiGateway> {
        self.pbx.gateway.read()
    }
}

#[tokio::test]
async fn test_comprehensive_core_event_structs() {
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║   Comprehensive Core RWI Event Struct Test                ║");
    println!("╚══════════════════════════════════════════════════════════╝\n");

    let ctx = TestCtx::new().await;
    let call_id = "comprehensive-core-call";
    let now = chrono::Utc::now().to_rfc3339();

    {
        let gw = ctx.gw();

        gw.fan_out(
            "default",
            &rustpbx::rwi::CallIncoming {
                call_id: call_id.into(),
                context: "default".into(),
                caller: "sip:alice@test.local".into(),
                callee: "sip:bob@test.local".into(),
                dial_direction: "inbound".into(),
                trunk: None,
                sip_headers: std::collections::HashMap::new(),
                root_call_id: None,
                caller_name: None,
                callee_name: None,
                called_phone: None,
                app_id: None,
                routing_target: None,
                uuid: None,
                routing_path: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::CallRinging {
                call_id: call_id.into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::CallAnswered {
                call_id: call_id.into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::RecordStarted {
                call_id: call_id.into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::RecordStopped {
                call_id: call_id.into(),
                duration_secs: Some(3),
                filename: Some("core.wav".into()),
                unique_id: Some("rec-core-001".into()),
                file_size: Some(1024),
                download_url: None,
                caller_name: None,
                callee_name: None,
                called_phone: None,
                call_type: None,
                agent_id: None,
                agent_name: None,
                call_start_time: None,
                call_end_time: None,
                upload_time: None,
                switch_flag: None,
                root_call_id: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::QueueJoined {
                call_id: call_id.into(),
                queue_id: "support".into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::QueueLeft {
                call_id: call_id.into(),
                queue_id: "support".into(),
                reason: Some("answered".into()),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::IvrStepTrace {
                call_id: call_id.into(),
                session_id: "ivr-session-001".into(),
                caller: "alice".into(),
                callee: "bob".into(),
                step_index: 1,
                event_type: "step".into(),
                event_detail: Some("menu".into()),
                action_type: "prompt".into(),
                action_json: None,
                result_kind: "ok".into(),
                duration_ms: 12,
                error: None,
                step_id: Some("step-1".into()),
                step_name: Some("Main menu".into()),
                step_start_time: Some(now.clone()),
                step_end_time: Some(now.clone()),
                extra: None,
                sip_headers: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::IvrNodeEntered {
                call_id: call_id.into(),
                node_id: "node-1".into(),
                node_name: "Main menu".into(),
                node_type: "menu".into(),
                app_id: "ivr-main".into(),
                entry_time: now.clone(),
                caller_name: None,
                callee_name: None,
                routing_target: None,
                previous_node_id: None,
                extra: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::IvrNodeExited {
                call_id: call_id.into(),
                node_id: "node-1".into(),
                node_name: "Main menu".into(),
                result_value: Some("1".into()),
                duration_ms: 25,
                exit_time: now.clone(),
                next_node_id: Some("node-2".into()),
                hangup_reason: None,
                call_result: None,
                extra: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::IvrFlowCompleted {
                call_id: call_id.into(),
                app_id: "ivr-main".into(),
                total_nodes_traversed: 2,
                total_duration_ms: 37,
                final_result: "completed".into(),
                completion_time: now,
                final_routing_target: Some("support".into()),
                extra: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::MediaHoldStarted {
                call_id: call_id.into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::MediaHoldStopped {
                call_id: call_id.into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::MediaPlayStarted {
                call_id: call_id.into(),
                leg_id: None,
                track_id: "prompt-1".into(),
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::MediaPlayFinished {
                call_id: call_id.into(),
                leg_id: None,
                track_id: "prompt-1".into(),
                interrupted: false,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::Dtmf {
                call_id: call_id.into(),
                digit: "1".into(),
                leg_id: None,
                extra: None,
            },
        );
        gw.fan_out(
            "default",
            &rustpbx::rwi::CallHangup {
                call_id: call_id.into(),
                reason: Some("normal".into()),
                sip_status: Some(200),
            },
        );

        gw.broadcast(&rustpbx::rwi::ConferenceCreated {
            conf_id: "conf-core-001".into(),
        });
        gw.broadcast(&rustpbx::rwi::CallBridged {
            leg_a: call_id.into(),
            leg_b: "agent-leg".into(),
        });
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let checks = [
        ("call_incoming", "call lifecycle"),
        ("call_ringing", "call lifecycle"),
        ("call_answered", "call lifecycle"),
        ("record_started", "recording"),
        ("record_stopped", "recording"),
        ("queue_joined", "queue"),
        ("queue_left", "queue"),
        ("ivr_step_trace", "ivr"),
        ("ivr_node_entered", "ivr"),
        ("ivr_node_exited", "ivr"),
        ("ivr_flow_completed", "ivr"),
        ("media_hold_started", "media"),
        ("media_hold_stopped", "media"),
        ("media_play_started", "media"),
        ("media_play_finished", "media"),
        ("dtmf", "dtmf"),
        ("call_hangup", "call lifecycle"),
        ("conference_created", "conference"),
        ("call_bridged", "conference"),
    ];

    println!("\n═══ Event Type Verification ═══");
    let mut total_received = 0;
    for (et, desc) in checks {
        let count = ctx.log.count(et).await;
        if count > 0 {
            println!("  {et: <30} x{count: <3}  ({desc})");
            total_received += 1;
        } else {
            println!("  {et: <30} x0    ({desc})");
        }
    }

    let events = ctx.log.snapshot().await;
    println!("\n═══ All Received Events (Full JSON) ═══");
    for (i, ev) in events.iter().enumerate() {
        let et = event_type_name(ev).await.unwrap_or_else(|| "?".to_string());
        println!(
            "\n[{i}] event_type={et}\n{}",
            serde_json::to_string_pretty(ev).unwrap_or_default()
        );
    }

    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!(
        "║  Results: {total_received}/{} event types received      ║",
        checks.len()
    );
    println!("╚══════════════════════════════════════════════════════════╝\n");

    assert_eq!(
        total_received,
        checks.len(),
        "not all core RWI event types were received"
    );

    ctx.pbx.stop();
}

// ═══════════════════════════════════════════════════════════════════════════
// New API demonstration: struct-based events + generic gateway methods
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_new_api_event_structs() {
    let _ = tracing_subscriber::fmt::try_init();

    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║   New API: struct-based events + generic gateway           ║");
    println!("║   Uses CallRinging / CallHangup structs + gw.broadcast()   ║");
    println!("╚══════════════════════════════════════════════════════════╝\n");

    let ctx = TestCtx::new().await;
    println!("PBX: RWI={}", ctx.pbx.rwi_url);

    let call_id = "new-api-demo-001";

    // ── 1. Use broadcast (new generic) ────────────────────────────
    println!("\n── Sending CallRinging via gw.broadcast (new generic) ──");
    {
        let gw = ctx.gw();
        gw.broadcast(&rustpbx::rwi::CallRinging {
            call_id: call_id.into(),
        });
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ── 2. CallHangup via broadcast (ownership not required) ────────
    println!("\n── Sending CallHangup via gw.broadcast (new generic) ──");
    {
        let gw = ctx.gw();
        gw.broadcast(&rustpbx::rwi::CallHangup {
            call_id: call_id.into(),
            reason: Some("normal".into()),
            sip_status: Some(200),
        });
    }

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ── 3. Fan out via new generic ────────────────────────────────────
    println!("\n── Sending CallAnswered via gw.fan_out (new generic) ──");
    {
        let gw = ctx.gw();
        gw.fan_out(
            "default",
            &rustpbx::rwi::CallAnswered {
                call_id: call_id.into(),
            },
        );
    }

    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // ── 4. Verify events arrived on WS ───────────────────────────────
    let events = ctx.log.snapshot().await;
    println!("\n═══ Events Received ═══");
    for (i, ev) in events.iter().enumerate() {
        let et = ev.get("event_type").and_then(|s| s.as_str()).unwrap_or("?");
        let detail: String = serde_json::to_string(ev)
            .unwrap_or_default()
            .chars()
            .take(250)
            .collect();
        println!("  [{i}] event_type={et} | {detail}");
    }

    let ringing_count = events
        .iter()
        .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some("call_ringing"))
        .count();
    let hangup_count = events
        .iter()
        .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some("call_hangup"))
        .count();
    let answered_count = events
        .iter()
        .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some("call_answered"))
        .count();

    println!("\n═══ Verification ═══");
    println!(
        "  call_ringing  x{ringing_count}  {}",
        if ringing_count > 0 { "✅" } else { "❌" }
    );
    println!(
        "  call_hangup   x{hangup_count}  {}",
        if hangup_count > 0 { "✅" } else { "❌" }
    );
    println!(
        "  call_answered x{answered_count}  {}",
        if answered_count > 0 { "✅" } else { "❌" }
    );

    assert!(
        ringing_count > 0,
        "call_ringing from new struct + new gateway method"
    );
    assert!(
        hangup_count > 0,
        "call_hangup from new struct + new gateway method"
    );
    assert!(
        answered_count > 0,
        "call_answered from new struct + new gateway method"
    );

    println!("\n✅ New API: struct-based RwiEventSpec + generic gateway methods verified!\n");

    ctx.pbx.stop();
}

// ═══════════════════════════════════════════════════════════════════════════
// CC addon event structs (new API)
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "addon-cc")]
#[tokio::test]
async fn test_new_cc_event_structs() {
    let _ = tracing_subscriber::fmt::try_init();

    let ctx = TestCtx::new().await;

    let gw = ctx.gw();
    gw.broadcast(&rustpbx::addons::cc::cc_events::AgentStateChanged {
        agent_id: "a1".into(),
        from_status: "offline".into(),
        to_status: "idle".into(),
        call_id: None,
        agent_name: None,
        agent_extension: None,
        caller: None,
        team_id: None,
        duration_secs: None,
        reason_code: Some("registered".into()),
    });
    gw.broadcast(&rustpbx::addons::cc::cc_events::QueueJoined {
        call_id: "c1".into(),
        queue_id: "q1".into(),
    });
    gw.broadcast(&rustpbx::addons::cc::cc_events::AgentRegistered {
        agent_id: "a2".into(),
        agent_name: None,
        agent_extension: None,
        team_id: None,
    });
    drop(gw);

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let events = ctx.log.snapshot().await;
    let r1 = events
        .iter()
        .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some("agent_state_changed"))
        .count();
    let r2 = events
        .iter()
        .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some("queue_joined"))
        .count();
    let r3 = events
        .iter()
        .filter(|v| v.get("event_type").and_then(|s| s.as_str()) == Some("agent_registered"))
        .count();

    assert!(r1 > 0, "agent_state_changed not received");
    assert!(r2 > 0, "queue_joined not received");
    assert!(r3 > 0, "agent_registered not received");

    println!(
        "  CC events: agent_state_changed x{r1}, queue_joined x{r2}, agent_registered x{r3} ✅"
    );
    ctx.pbx.stop();
}
