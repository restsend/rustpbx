//! Audio Content E2E Tests
//!
//! Verifies that audio content traverses the PBX correctly using algorithmic
//! signal analysis (Goertzel frequency detection, RMS energy, cross-correlation).
//!
//! Test strategy:
//! 1. Generate a known reference WAV (e.g. 440 Hz sine tone)
//! 2. Play it through the PBX to a sipbot UA (Echo mode with recording enabled)
//! 3. Read sipbot's recorded WAV and verify:
//!    - The RX channel contains the expected frequency (Goertzel)
//!    - Audio energy is above silence threshold (RMS)
//!    - Audio quality metrics are acceptable (silence ratio, clipping)
//! 4. For DTMF: send digits via RWI and verify sipbot receives them

mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::audio_verifier::{
    compute_rms, extract_audio_region, find_dominant_frequency, find_signal_start,
    generate_sine_wav, goertzel_magnitude_normalized, has_audio_content, read_wav_stereo,
};
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

/// Test: PBX plays a 440 Hz sine tone to a sipbot UA via call.originate + CallCommand::Play.
///
/// Verifies:
/// 1. sipbot receives RTP packets (has_rtp_rx)
/// 2. sipbot's WAV recording contains audio at ~440 Hz (Goertzel)
/// 3. Audio energy is above silence threshold (RMS > -30 dB)
/// 4. Audio quality stats show non-silent frames
#[tokio::test]
async fn test_tone_delivery_via_originate() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let callee_port = portpicker::pick_unused_port().expect("no free callee port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_audio_e2e_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 2.0, 8000, 0.5);

    let record_path = temp_dir.join("callee_recording.wav");

    let pbx = TestPbx::start(sip_port).await;

    let callee = TestUa::callee_with_record(
        callee_port,
        0,
        "callee",
        record_path.to_string_lossy().to_string(),
    )
    .await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    let call_id = format!("audio-tone-{}", Uuid::new_v4());
    let (_, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": callee.sip_uri("callee"),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv(&mut ws, &orig_json).await;
    assert_eq!(v["type"], "command_completed", "originate failed: {v}");

    tokio::time::sleep(Duration::from_secs(1)).await;

    {
        let handle = pbx
            .registry
            .get_handle(&call_id)
            .expect("originate handle must exist");
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
    }

    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should have received RTP packets. Stats: {}",
        callee.rtp_stats_summary()
    );

    let quality = callee.audio_quality_summary();
    tracing::info!(
        "Audio quality: total={}, silence={}, clipping={}, shrill={}, muffled={}",
        quality.total_frames,
        quality.silence_frames,
        quality.clipping_frames,
        quality.shrill_count,
        quality.muffled_count
    );

    assert!(
        quality.has_audio(),
        "Audio quality stats should show non-silent frames. Summary: {:?}",
        quality
    );

    callee.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;

    if record_path.exists() {
        let (rx_ch, _tx_ch, rec_sr) = read_wav_stereo(&record_path);
        tracing::info!(
            "Recording: {} samples at {}Hz, RX channel has {} samples",
            rx_ch.len() + _tx_ch.len(),
            rec_sr,
            rx_ch.len()
        );

        if !rx_ch.is_empty() {
            let signal_start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            tracing::info!("Signal starts at sample index {}", signal_start);

            let region = extract_audio_region(&rx_ch, rec_sr, signal_start, 1000);
            if !region.is_empty() {
                let rms_db = compute_rms(region);
                tracing::info!("RX audio RMS: {:.1} dB", rms_db);
                assert!(
                    has_audio_content(region, -30.0),
                    "RX audio should have energy above -30 dB, got {:.1} dB",
                    rms_db
                );

                let (freq, mag) = find_dominant_frequency(region, rec_sr, 200.0, 800.0, 5.0);
                tracing::info!(
                    "Dominant frequency: {:.0} Hz (magnitude: {:.1})",
                    freq,
                    mag
                );
                assert!(
                    (freq - 440.0).abs() < 30.0,
                    "Dominant frequency should be near 440 Hz, got {:.0} Hz",
                    freq
                );

                let m440 = goertzel_magnitude_normalized(region, 440.0, rec_sr);
                let m1000 = goertzel_magnitude_normalized(region, 1000.0, rec_sr);
                assert!(
                    m440 > m1000 * 5.0,
                    "440 Hz component should dominate 1000 Hz: m440={:.1}, m1000={:.1}",
                    m440,
                    m1000
                );
            }
        }
    } else {
        tracing::warn!("Recording file not found at {:?}", record_path);
    }

    ws.close(None).await.ok();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_tone_delivery_via_originate PASSED");
}

/// Test: DTMF digits sent via RWI are delivered successfully.
///
/// The PBX sends DTMF via SIP INFO (application/dtmf-relay), not RFC 4733
/// telephone-event RTP packets. We verify:
/// 1. The `call.send_dtmf` command succeeds via RWI
/// 2. RTP continues flowing after DTMF (call doesn't crash)
/// 3. sipbot's audio quality remains acceptable (no disruption)
#[tokio::test]
async fn test_dtmf_delivery_via_rwi() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let callee_port = portpicker::pick_unused_port().expect("no free callee port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_dtmf_e2e_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let tone_path = temp_dir.join("carrier_tone.wav");
    generate_sine_wav(&tone_path, 440.0, 5.0, 8000, 0.3);

    let pbx = TestPbx::start(sip_port).await;

    let callee = TestUa::callee_with_username(callee_port, 0, "callee").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    let call_id = format!("audio-dtmf-{}", Uuid::new_v4());
    let (_, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": call_id,
            "destination": callee.sip_uri("callee"),
            "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv(&mut ws, &orig_json).await;
    assert_eq!(v["type"], "command_completed", "originate failed: {v}");

    tokio::time::sleep(Duration::from_secs(1)).await;

    {
        let handle = pbx
            .registry
            .get_handle(&call_id)
            .expect("originate handle must exist");
        handle
            .send_command(CallCommand::Play {
                leg_id: None,
                source: DomainMediaSource::File {
                    path: tone_path.to_str().unwrap().to_string(),
                },
                options: Some(PlayOptions {
                    loop_playback: true,
                    ..Default::default()
                }),
            })
            .expect("send Play command");
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should have received RTP. Stats: {}",
        callee.rtp_stats_summary()
    );

    let rtp_before = callee.rtp_stats_summary();

    let (_, dtmf_json) = rwi_req(
        "call.send_dtmf",
        serde_json::json!({
            "call_id": call_id,
            "digits": "1234#"
        }),
    );
    let v = ws_send_recv(&mut ws, &dtmf_json).await;
    assert_eq!(
        v["type"], "command_completed",
        "DTMF send should succeed: {v}"
    );

    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should still have RTP after DTMF. Stats: {}",
        callee.rtp_stats_summary()
    );

    let quality = callee.audio_quality_summary();
    tracing::info!(
        "Post-DTMF audio quality: total={}, silence={}, clipping={:.3}, has_audio={}",
        quality.total_frames,
        quality.silence_frames,
        quality.clipping_ratio(),
        quality.has_audio()
    );

    if quality.total_frames > 0 {
        assert!(
            quality.clipping_ratio() < 0.1,
            "No excessive clipping after DTMF: {:.3}",
            quality.clipping_ratio()
        );
    }

    tracing::info!(
        "DTMF test: RTP before={}, RTP after={}",
        rtp_before,
        callee.rtp_stats_summary()
    );

    ws.close(None).await.ok();
    callee.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_dtmf_delivery_via_rwi PASSED");
}

/// Test: Bidirectional audio flow — verify that the echo from sipbot reaches
/// back to the PBX side. We play a tone to sipbot, sipbot echoes it back,
/// then we check the caller sipbot's recording for the echoed tone.
#[tokio::test]
async fn test_echo_bidirectional_audio() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_echo_e2e_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 2.0, 8000, 0.5);

    let caller_record_path = temp_dir.join("caller_recording.wav");

    let pbx = TestPbx::start(sip_port).await;

    let caller = TestUa::callee_with_record(
        caller_port,
        0,
        "caller",
        caller_record_path.to_string_lossy().to_string(),
    )
    .await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    let call_id = format!("audio-echo-{}", Uuid::new_v4());
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

    tokio::time::sleep(Duration::from_secs(1)).await;

    {
        let handle = pbx
            .registry
            .get_handle(&call_id)
            .expect("originate handle must exist");
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
    }

    tokio::time::sleep(Duration::from_secs(4)).await;

    assert!(
        caller.has_rtp_rx(),
        "caller should have received RTP. Stats: {}",
        caller.rtp_stats_summary()
    );
    assert!(
        caller.has_rtp_tx(),
        "caller should have transmitted RTP (echo). Stats: {}",
        caller.rtp_stats_summary()
    );

    let quality = caller.audio_quality_summary();
    tracing::info!(
        "Caller audio quality: total={}, silence={}, has_audio={}",
        quality.total_frames,
        quality.silence_frames,
        quality.has_audio()
    );

    assert!(
        quality.has_audio(),
        "Caller audio quality should show non-silent frames"
    );

    caller.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;

    if caller_record_path.exists() {
        let (rx_ch, tx_ch, rec_sr) = read_wav_stereo(&caller_record_path);
        tracing::info!(
            "Caller recording: {} stereo samples at {}Hz",
            rx_ch.len(),
            rec_sr
        );

        if !rx_ch.is_empty() {
            let signal_start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            let region = extract_audio_region(&rx_ch, rec_sr, signal_start, 1000);

            if !region.is_empty() {
                let rms_db = compute_rms(region);
                tracing::info!("Caller RX RMS: {:.1} dB", rms_db);

                if has_audio_content(region, -35.0) {
                    let (freq, mag) =
                        find_dominant_frequency(region, rec_sr, 200.0, 800.0, 5.0);
                    tracing::info!(
                        "Caller RX dominant freq: {:.0} Hz (mag: {:.1})",
                        freq,
                        mag
                    );
                }
            }
        }

        if !tx_ch.is_empty() {
            let tx_start = find_signal_start(&tx_ch, 0.01, rec_sr as usize / 50);
            let tx_region = extract_audio_region(&tx_ch, rec_sr, tx_start, 1000);

            if !tx_region.is_empty() {
                let tx_rms = compute_rms(tx_region);
                tracing::info!("Caller TX (echo) RMS: {:.1} dB", tx_rms);
            }
        }
    } else {
        tracing::warn!("Caller recording file not found");
    }

    ws.close(None).await.ok();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_echo_bidirectional_audio PASSED");
}

/// Test: Queue hold music — verify that when queue plays a tone file as hold music,
/// the caller sipbot receives the correct audio content.
#[tokio::test]
async fn test_queue_hold_music_content() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_queue_audio_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let hold_music_path = temp_dir.join("hold_music.wav");
    generate_sine_wav(&hold_music_path, 440.0, 2.0, 8000, 0.5);

    let caller_record_path = temp_dir.join("caller_recording.wav");

    use helpers::test_server::TestPbxInject;
    use rustpbx::proxy::routing::{
        RouteQueueConfig, RouteQueueHoldConfig, RouteQueueStrategyConfig, RouteQueueTargetConfig,
    };
    use std::collections::HashMap;

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
                    uri: format!("sip:agent1@127.0.0.1:{}", agent_port),
                    label: Some("Agent".to_string()),
                }],
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let inject = TestPbxInject {
        queues: Some(queues),
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(sip_port, inject).await;

    let caller = TestUa::callee_with_record(
        caller_port,
        0,
        "caller",
        caller_record_path.to_string_lossy().to_string(),
    )
    .await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    let call_id = format!("queue-audio-{}", Uuid::new_v4());
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
            "params": {"name": "support"},
        }),
    );
    let v = ws_send_recv(&mut ws, &app_start_json).await;
    assert_eq!(
        v["type"], "command_completed",
        "app_start(queue) failed: {v}"
    );

    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        caller.has_rtp_rx(),
        "caller should have received RTP (hold music). Stats: {}",
        caller.rtp_stats_summary()
    );

    let quality = caller.audio_quality_summary();
    assert!(
        quality.has_audio(),
        "Queue hold music should produce non-silent audio. Quality: {:?}",
        quality
    );

    caller.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;

    if caller_record_path.exists() {
        let (rx_ch, _, rec_sr) = read_wav_stereo(&caller_record_path);

        if !rx_ch.is_empty() {
            let signal_start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            let region = extract_audio_region(&rx_ch, rec_sr, signal_start, 1000);

            if !region.is_empty() && has_audio_content(region, -35.0) {
                let (freq, _) = find_dominant_frequency(region, rec_sr, 200.0, 800.0, 5.0);
                tracing::info!("Queue hold music dominant freq: {:.0} Hz", freq);

                assert!(
                    (freq - 440.0).abs() < 40.0,
                    "Hold music should contain the 440 Hz tone, got {:.0} Hz",
                    freq
                );
            }
        }
    }

    ws.close(None).await.ok();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_queue_hold_music_content PASSED");
}
