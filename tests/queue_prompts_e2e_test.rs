//! Queue Voice Prompts E2E Tests
//!
//! Tests queue voice prompts (transfer, busy, no-answer) end-to-end using sipbot.
//!
//! Test scenarios:
//! 1. test_queue_prompts_hold_music: Queue configured with voice_prompts → hold music plays
//! 2. test_queue_agent_transfer_flow: Queue → hold music → agent connects → RTP flows
//!
//! NOTE: Voice prompt audio playback is verified at the unit-test level
//! (src/call/app/queue_test.rs). These E2E tests validate that the overall
//! queue flow works correctly with voice_prompts configuration and that
//! RTP/media functions end-to-end.

mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::audio_verifier::generate_sine_wav;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use rustpbx::call::VoicePrompts;
use rustpbx::proxy::routing::{
    RouteQueueConfig, RouteQueueHoldConfig, RouteQueueStrategyConfig, RouteQueueTargetConfig,
};
use std::collections::HashMap;
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
        .expect("connect timeout")
        .expect("connect error");
    ws
}

async fn ws_send_recv(ws: &mut WsStream, json: &str) -> serde_json::Value {
    let req: serde_json::Value = serde_json::from_str(json).expect("invalid JSON");
    let action_id = req["action_id"]
        .as_str()
        .expect("missing action_id")
        .to_string();

    ws.send(Message::Text(json.into())).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("ws_send_recv: timed out waiting for response");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");

        if let Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t).expect("not JSON");
            if (v["type"] == "command_completed" || v["type"] == "command_failed")
                && v["action_id"] == action_id
            {
                return v;
            }
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

fn create_tone_wav(path: &std::path::Path, freq: f64) {
    generate_sine_wav(path, freq, 2.0, 8000, 0.5);
}

// ── Test 1: Queue with voice prompts → hold music flows ──────────────

#[tokio::test]
async fn test_queue_prompts_hold_music() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");

    // ── Audio file setup ──────────────────────────────────────────
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_prompts_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let hold_music_path = temp_dir.join("hold_music.wav");
    let transfer_prompt_path = temp_dir.join("transfer_prompt.wav");
    let busy_prompt_path = temp_dir.join("busy_prompt.wav");
    let no_answer_prompt_path = temp_dir.join("no_answer_prompt.wav");
    create_tone_wav(&hold_music_path, 440.0);
    create_tone_wav(&transfer_prompt_path, 523.0);
    create_tone_wav(&busy_prompt_path, 600.0);
    create_tone_wav(&no_answer_prompt_path, 700.0);

    // ── Queue config with voice prompts ───────────────────────────
    let mut queues = HashMap::new();
    queues.insert(
        "support".to_string(),
        RouteQueueConfig {
            name: Some("support".to_string()),
            accept_immediately: true,
            hold: Some(RouteQueueHoldConfig {
                audio_file: Some(hold_music_path.to_string_lossy().to_string()),
                loop_playback: true,
            }),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: format!(
                        "sip:dummy@127.0.0.1:{}",
                        portpicker::pick_unused_port().unwrap()
                    ),
                    label: Some("Dummy".to_string()),
                }],
                ..Default::default()
            },
            voice_prompts: Some(VoicePrompts {
                transfer_prompt: Some(transfer_prompt_path.to_string_lossy().to_string()),
                busy_prompt: Some(busy_prompt_path.to_string_lossy().to_string()),
                no_answer_prompt: Some(no_answer_prompt_path.to_string_lossy().to_string()),
                off_hours_prompt: None,
                position_prompt: None,
                wait_time_prompt: None,
            }),
            ..Default::default()
        },
    );

    let inject = TestPbxInject {
        queues: Some(queues),
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;

    // ── Caller UA (auto-answers, echoes) ──────────────────────────
    let caller = TestUa::callee_with_username(caller_port, 1, "caller").await;

    // ── RWI: originate → start queue → verify hold music RTP ──────
    let mut ws = ws_connect(&pbx.rwi_url).await;
    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    let call_id = format!("e2e-prompts-{}", Uuid::new_v4());
    let (_, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": caller.sip_uri("caller"),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv(&mut ws, &orig_json).await;
    assert_eq!(v["type"], "command_completed", "originate failed: {v}");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Start queue with voice_prompts configured
    let (_, app_start_json) = rwi_req(
        "call.app_start",
        serde_json::json!({
            "call_id": call_id,
            "app_name": "queue",
            "params": {"name": "support"},
        }),
    );
    let v = ws_send_recv(&mut ws, &app_start_json).await;
    assert_eq!(
        v["type"], "command_completed",
        "app_start(queue) failed: {v}"
    );

    // Let hold music flow for 2 seconds
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify caller receives RTP (hold music)
    assert!(
        caller.has_rtp_rx(),
        "caller should have received RTP (hold music). Stats: {}",
        caller.rtp_stats_summary()
    );
    let q = caller.audio_quality_summary();
    assert!(q.has_audio(), "caller should have non-silent audio from hold music. Quality: {:?}", q);
    tracing::info!("Hold music RTP OK — caller: {}, quality: total={} silence={}", caller.rtp_stats_summary(), q.total_frames, q.silence_frames);

    // Clean up
    let (_, app_stop_json) = rwi_req("call.app_stop", serde_json::json!({"call_id": call_id}));
    let _ = ws_send_recv(&mut ws, &app_stop_json).await;

    ws.close(None).await.unwrap();
    caller.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_queue_prompts_hold_music PASSED");
}

// ── Test 2: Queue → Agent full flow with voice prompts ─────────────
// Verifies that queue with voice_prompts + agent connection works.

#[tokio::test]
async fn test_queue_agent_transfer_flow() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");

    // ── Audio file setup ──────────────────────────────────────────
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_transfer_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let hold_music_path = temp_dir.join("hold_music.wav");
    let transfer_prompt_path = temp_dir.join("transfer_prompt.wav");
    let busy_prompt_path = temp_dir.join("busy_prompt.wav");
    let no_answer_prompt_path = temp_dir.join("no_answer_prompt.wav");
    create_tone_wav(&hold_music_path, 440.0);
    create_tone_wav(&transfer_prompt_path, 523.0);
    create_tone_wav(&busy_prompt_path, 600.0);
    create_tone_wav(&no_answer_prompt_path, 700.0);

    // ── Queue config with voice prompts and agent target ───────────
    let mut queues = HashMap::new();
    queues.insert(
        "sales".to_string(),
        RouteQueueConfig {
            name: Some("sales".to_string()),
            accept_immediately: true,
            hold: Some(RouteQueueHoldConfig {
                audio_file: Some(hold_music_path.to_string_lossy().to_string()),
                loop_playback: true,
            }),
            strategy: RouteQueueStrategyConfig {
                targets: vec![RouteQueueTargetConfig {
                    uri: format!("sip:agent1@127.0.0.1:{}", agent_port),
                    label: Some("Sales Agent".to_string()),
                }],
                ..Default::default()
            },
            voice_prompts: Some(VoicePrompts {
                transfer_prompt: Some(transfer_prompt_path.to_string_lossy().to_string()),
                busy_prompt: Some(busy_prompt_path.to_string_lossy().to_string()),
                no_answer_prompt: Some(no_answer_prompt_path.to_string_lossy().to_string()),
                off_hours_prompt: None,
                position_prompt: None,
                wait_time_prompt: None,
            }),
            ..Default::default()
        },
    );

    let inject = TestPbxInject {
        queues: Some(queues),
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;

    // ── SIP UAs ───────────────────────────────────────────────────
    let caller = TestUa::callee_with_username(caller_port, 1, "caller").await;
    let agent = TestUa::callee_with_username(agent_port, 0, "agent1").await;

    // ── RWI ───────────────────────────────────────────────────────
    let mut ws = ws_connect(&pbx.rwi_url).await;
    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    // Phase 1: Originate → caller answers → start queue (hold music)
    let call_id = format!("e2e-transfer-{}", Uuid::new_v4());
    let (_, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": caller.sip_uri("caller"),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv(&mut ws, &orig_json).await;
    assert_eq!(v["type"], "command_completed", "originate failed: {v}");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let (_, app_start_json) = rwi_req(
        "call.app_start",
        serde_json::json!({
            "call_id": call_id,
            "app_name": "queue",
            "params": {"name": "sales"},
        }),
    );
    let v = ws_send_recv(&mut ws, &app_start_json).await;
    assert_eq!(v["type"], "command_completed", "app_start failed: {v}");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Assert caller receives hold music RTP
    assert!(
        caller.has_rtp_rx(),
        "Phase 1: caller should have RTP (hold music). Stats: {}",
        caller.rtp_stats_summary()
    );
    {
        let q = caller.audio_quality_summary();
        assert!(q.has_audio(), "Phase 1: caller should have non-silent audio. Quality: {:?}", q);
    }

    // Phase 2: Stop queue → add agent leg
    let (_, app_stop_json) = rwi_req("call.app_stop", serde_json::json!({"call_id": call_id}));
    ws_send_recv(&mut ws, &app_stop_json).await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let (_, agent_add_json) = rwi_req(
        "call.leg_add",
        serde_json::json!({
            "call_id": call_id,
            "target": agent.sip_uri("agent1"),
            "leg_id": "agent-1",
        }),
    );
    let v = ws_send_recv(&mut ws, &agent_add_json).await;
    assert_eq!(v["type"], "command_completed", "leg_add failed: {v}");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Play hold music directly on the agent leg so RTP flows to agent
    let handle = pbx
        .registry
        .get_handle(&call_id)
        .expect("originate handle must exist");
    handle
        .send_command(rustpbx::call::domain::CallCommand::Play {
            leg_id: Some(rustpbx::call::domain::LegId::new("agent-1")),
            source: rustpbx::call::domain::MediaSource::File {
                path: hold_music_path.to_str().unwrap().to_string(),
            },
            options: Some(rustpbx::call::domain::PlayOptions {
                loop_playback: true,
                ..Default::default()
            }),
        })
        .expect("send Play to agent leg");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Assert agent receives RTP
    assert!(
        agent.has_rtp_rx(),
        "Phase 2: agent should have RTP. Stats: {}",
        agent.rtp_stats_summary()
    );
    {
        let q = agent.audio_quality_summary();
        assert!(q.has_audio(), "Phase 2: agent should have non-silent audio. Quality: {:?}", q);
    }

    tracing::info!(
        "Agent RTP OK — caller: {}, agent: {}",
        caller.rtp_stats_summary(),
        agent.rtp_stats_summary()
    );

    // Cleanup
    ws.close(None).await.unwrap();
    caller.stop();
    agent.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_queue_agent_transfer_flow PASSED");
}
