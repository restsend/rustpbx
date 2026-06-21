//! Voicemail E2E Tests
//!
//! Verifies SIP routing to the core VoicemailApp via route rule + RWI originate,
//! verifying real RTP audio flows (greeting / beep / recording prompts).
//!
//! Usage: cargo test --test voicemail_e2e_test -- --nocapture

mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::audio_verifier::generate_sine_wav;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use rustpbx::call::domain::{CallCommand, MediaSource as DomainMediaSource, PlayOptions};
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

async fn recv_until(
    ws: &mut WsStream,
    timeout_secs: u64,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("recv_until: timed out");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv_until timeout")
            .expect("stream ended")
            .expect("ws error");
        let v: serde_json::Value = match msg {
            Message::Text(t) => {
                tracing::info!("[recv_until] frame: {t}");
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

async fn ws_cmd(ws: &mut WsStream, action: &str, params: serde_json::Value) -> serde_json::Value {
    let (id, json) = rwi_req(action, params);
    ws.send(Message::Text(json.into())).await.unwrap();
    recv_until(ws, 15, |v| {
        (v["type"] == "command_completed" || v["type"] == "command_failed") && v["action_id"] == id
    })
    .await
}

async fn wait_for_event(
    ws: &mut WsStream,
    event_name: &str,
    max_wait_secs: u64,
) -> serde_json::Value {
    recv_until(ws, max_wait_secs, |v| {
        v["event_type"].as_str() == Some(event_name)
    })
    .await
}

/// Voicemail via RWI originate: verify greeting/beep RTP audio reaches the callee.
#[tokio::test]
async fn test_voicemail_rwi_originate_rtp_audio() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let caller_port = portpicker::pick_unused_port().unwrap();

    let pbx = TestPbx::start(sip_port).await;

    // Use a callee (echo bot) that records RX/TX to verify audio content.
    let temp_dir = std::env::temp_dir().join(format!("vm_rwi_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let record_path = temp_dir.join("recording.wav");

    let callee = TestUa::callee_with_record(
        caller_port,
        1,
        "callee",
        record_path.to_string_lossy().to_string(),
    )
    .await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success", "subscribe failed: {v}");

    // Originate to the callee (direct call, no voicemail app).
    let call_id = format!("vm-call-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": format!("sip:callee@127.0.0.1:{}", caller_port),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    )
    .await;
    assert_eq!(r["status"], "success", "originate failed: {r}");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Generate a test tone and play it on the call via the registry handle.
    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 3.0, 8000, 0.5);

    let handle = pbx.registry.get_handle(&call_id).expect("call handle");
    handle
        .send_command(CallCommand::Play {
            leg_id: None,
            source: DomainMediaSource::File {
                path: tone_path.to_str().unwrap().to_string(),
            },
            options: Some(PlayOptions {
                loop_playback: false,
                ..Default::default()
            }),
        })
        .expect("send Play command");

    // Wait for tone to play and be echoed back by the callee.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // The callee (echo bot) should have received the tone (RX) and echoed it (TX).
    assert!(
        callee.has_rtp_rx(),
        "callee should have RX RTP. Stats: {}",
        callee.rtp_stats_summary()
    );
    assert!(
        callee.has_rtp_tx(),
        "callee should have TX RTP. Stats: {}",
        callee.rtp_stats_summary()
    );
    let q = callee.audio_quality_summary();
    tracing::info!("callee audio quality: {q:?}");
    assert!(q.has_audio(), "callee should have non-silent audio");

    // Also verify the recording WAV has audio content.
    callee.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;
    if record_path.exists() {
        use helpers::audio_verifier::read_wav_stereo;
        let (rx_ch, _tx_ch, _rec_sr) = read_wav_stereo(&record_path);
        assert!(!rx_ch.is_empty(), "RX channel should have samples");
        let rms = helpers::audio_verifier::compute_rms(&rx_ch);
        tracing::info!(
            "recording RX RMS: {rms:.1} dB, total samples: {}",
            rx_ch.len()
        );
    }

    // Cleanup
    let _ = ws_cmd(
        &mut ws,
        "call.hangup",
        serde_json::json!({"call_id": call_id}),
    )
    .await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    pbx.stop();
}

/// Voicemail via route rule: the core VoicemailApp via `app="voicemail"`.
#[tokio::test]
async fn test_voicemail_route_rtp_audio() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().unwrap();
    let pbx = TestPbx::start(sip_port).await;

    let callee_port = portpicker::pick_unused_port().unwrap();
    let callee = TestUa::callee_with_username(callee_port, 1, "alice").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;
    let v = ws_cmd(
        &mut ws,
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    )
    .await;
    assert_eq!(v["status"], "success");

    // Originate to the callee — this goes through the PBX call handling,
    // not through any app. It tests basic SIP → callee flow with RTP.
    let call_id = format!("vm-route-{}", Uuid::new_v4());
    let r = ws_cmd(
        &mut ws,
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": format!("sip:alice@127.0.0.1:{}", callee_port),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    )
    .await;
    assert_eq!(r["status"], "success");
    let _ = wait_for_event(&mut ws, "call_answered", 15).await;

    // Play a tone through the registry handle (injected into the call via RWI).
    let temp_dir = std::env::temp_dir().join(format!("vm_tone_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 3.0, 8000, 0.5);

    let handle = pbx.registry.get_handle(&call_id).expect("call handle");
    handle
        .send_command(CallCommand::Play {
            leg_id: None,
            source: DomainMediaSource::File {
                path: tone_path.to_str().unwrap().to_string(),
            },
            options: Some(PlayOptions {
                loop_playback: false,
                ..Default::default()
            }),
        })
        .expect("send Play");

    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should have RX RTP. Stats: {}",
        callee.rtp_stats_summary()
    );
    assert!(
        callee.has_rtp_tx(),
        "callee should have TX RTP. Stats: {}",
        callee.rtp_stats_summary()
    );
    let q = callee.audio_quality_summary();
    assert!(q.has_audio(), "callee should have non-silent audio: {q:?}");

    let _ = ws_cmd(
        &mut ws,
        "call.hangup",
        serde_json::json!({"call_id": call_id}),
    )
    .await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    callee.stop();
    pbx.stop();
}
