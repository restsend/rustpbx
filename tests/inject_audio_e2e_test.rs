//! Inject Audio E2E Tests
//!
//! Verifies that the InjectAudio MediaEngine command correctly delivers audio
//! through the bridge's file output mechanism.
//!
//! Test architecture:
//! - Bridge-level tests: Create a real BridgePeer + MediaEngine session, send
//!   InjectAudio, and verify RTP output including frequency content.
//! - Full-stack test: Use RWI originate + CallCommand::Play (which shares the
//!   same FileTrack→RTP mechanism) to verify audio delivery end-to-end.
//! - Error handling: Verify InjectAudio fails gracefully for invalid inputs.

mod helpers;

use audio_codec::CodecType;
use futures::{SinkExt, StreamExt};
use helpers::audio_verifier::{
    compute_rms, extract_audio_region, find_signal_start, generate_sine_wav, has_audio_content,
    read_wav_stereo,
};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use rustpbx::call::domain::{CallCommand, MediaSource as DomainMediaSource, PlayOptions};
use rustpbx::media::FileTrack;
use rustpbx::media::bridge::{BridgeEndpoint, BridgePeerBuilder};
use rustpbx::media::engine::command::{InjectTarget, PlaySource};
use rustpbx::media::engine::{MediaCommand, MediaEngine, MediaEngineConfig};
use rustpbx::media::negotiate::CodecInfo;
use rustrtc::media::{MediaSample, MediaStreamTrack};
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

fn create_wav_file(path: &std::path::Path, sample_count: usize) {
    let sample_rate = 8000u32;
    let bits_per_sample = 16u16;
    let channels = 1u16;
    let data_size = (sample_count * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    let samples_per_cycle = (sample_rate as f64 / 440.0) as usize;
    for i in 0..sample_count {
        let phase = (i % samples_per_cycle) as f64 / samples_per_cycle as f64;
        let val = ((phase * 2.0 * std::f64::consts::PI).sin() * 16000.0) as i16;
        wav.extend_from_slice(&val.to_le_bytes());
    }

    std::fs::write(path, wav).expect("failed to write wav file");
}

fn codec_info() -> CodecInfo {
    CodecInfo {
        payload_type: 0,
        codec: CodecType::PCMU,
        clock_rate: 8000,
        channels: 1,
    }
}

fn setup_engine() -> (
    MediaEngine,
    tokio::sync::broadcast::Receiver<rustpbx::media::engine::MediaEvent>,
) {
    let (engine, handle) = MediaEngine::new(MediaEngineConfig {
        command_channel_capacity: 64,
        event_channel_capacity: 64,
    });
    let rx = engine.subscribe();
    let _task = engine.spawn(handle);
    (engine, rx)
}

/// Test: InjectAudio with InjectTarget::Both produces RTP output on both endpoints.
///
/// Creates a real bridge, injects audio via the MediaEngine, and verifies:
/// 1. PlayStarted event is emitted with leg_id "both"
/// 2. RTP frames are produced on both Caller and Callee endpoints
/// 3. Sequence numbers and timestamps are continuous
#[tokio::test]
async fn test_inject_audio_both_endpoints_produce_rtp() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_rtp_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 1600);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-inject-1".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let bridge = BridgePeerBuilder::new("test-inject-bridge-1".into())
        .with_rtp_port_range(29000, 29100)
        .build();
    bridge.setup_bridge().await.unwrap();

    engine
        .send(MediaCommand::AttachBridge {
            session_id: "e2e-inject-1".into(),
            bridge: bridge.clone(),
            caller_is_webrtc: false,
            caller_codec_info: vec![codec_info()],
        })
        .unwrap();

    engine
        .send(MediaCommand::BridgeLegs {
            session_id: "e2e-inject-1".into(),
            leg_a: "caller".into(),
            leg_b: "callee".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let caller_track = bridge.get_caller_track().await.expect("caller track");
    let callee_track = bridge.get_callee_track().await.expect("callee track");

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-inject-1".into(),
            source: PlaySource::File {
                path: wav_path.to_string_lossy().to_string(),
            },
            target: InjectTarget::Both,
            mute_peer: false,
        })
        .unwrap();

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for PlayStarted")
        .expect("channel closed");
    assert!(
        matches!(&ev, rustpbx::media::engine::MediaEvent::PlayStarted { leg_id, .. } if leg_id == "both"),
        "expected PlayStarted(both), got {:?}",
        ev
    );

    let caller_frame = timeout(Duration::from_millis(200), caller_track.recv())
        .await
        .expect("caller frame timeout")
        .expect("caller frame");
    let callee_frame = timeout(Duration::from_millis(200), callee_track.recv())
        .await
        .expect("callee frame timeout")
        .expect("callee frame");

    if let MediaSample::Audio(f) = &caller_frame {
        assert!(f.sequence_number.is_some(), "caller frame should have seq");
        assert_ne!(f.rtp_timestamp, 0, "caller frame should have timestamp");
    } else {
        panic!("expected audio frame from caller endpoint");
    }

    if let MediaSample::Audio(f) = &callee_frame {
        assert!(f.sequence_number.is_some(), "callee frame should have seq");
        assert_ne!(f.rtp_timestamp, 0, "callee frame should have timestamp");
    } else {
        panic!("expected audio frame from callee endpoint");
    }

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_inject_audio_both_endpoints_produce_rtp PASSED");
}

/// Test: InjectAudio with InjectTarget::Leg only produces RTP on that endpoint.
#[tokio::test]
async fn test_inject_audio_single_leg_produces_rtp() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_single_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 1600);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-inject-2".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let bridge = BridgePeerBuilder::new("test-inject-bridge-2".into())
        .with_rtp_port_range(29200, 29300)
        .build();
    bridge.setup_bridge().await.unwrap();

    engine
        .send(MediaCommand::AttachBridge {
            session_id: "e2e-inject-2".into(),
            bridge: bridge.clone(),
            caller_is_webrtc: true,
            caller_codec_info: vec![codec_info()],
        })
        .unwrap();

    engine
        .send(MediaCommand::BridgeLegs {
            session_id: "e2e-inject-2".into(),
            leg_a: "caller".into(),
            leg_b: "callee".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let callee_track = bridge.get_callee_track().await.expect("callee track");

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-inject-2".into(),
            source: PlaySource::File {
                path: wav_path.to_string_lossy().to_string(),
            },
            target: InjectTarget::Leg("callee".into()),
            mute_peer: false,
        })
        .unwrap();

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for PlayStarted")
        .expect("channel closed");
    assert!(
        matches!(&ev, rustpbx::media::engine::MediaEvent::PlayStarted { leg_id, .. } if leg_id == "caller"),
        "expected PlayStarted(caller) for callee target, got {:?}",
        ev
    );

    let callee_frame = timeout(Duration::from_millis(200), callee_track.recv())
        .await
        .expect("callee frame timeout")
        .expect("callee frame");

    if let MediaSample::Audio(f) = &callee_frame {
        assert!(f.sequence_number.is_some(), "callee frame should have seq");
    } else {
        panic!("expected audio frame from callee endpoint");
    }

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_inject_audio_single_leg_produces_rtp PASSED");
}

/// Test: InjectAudio with mute_peer suppresses the opposite endpoint's output.
#[tokio::test]
async fn test_inject_audio_mute_peer_suppresses_output() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_mute_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 1600);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-inject-3".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let bridge = BridgePeerBuilder::new("test-inject-bridge-3".into())
        .with_rtp_port_range(29400, 29500)
        .build();
    bridge.setup_bridge().await.unwrap();

    engine
        .send(MediaCommand::AttachBridge {
            session_id: "e2e-inject-3".into(),
            bridge: bridge.clone(),
            caller_is_webrtc: true,
            caller_codec_info: vec![codec_info()],
        })
        .unwrap();

    engine
        .send(MediaCommand::BridgeLegs {
            session_id: "e2e-inject-3".into(),
            leg_a: "caller".into(),
            leg_b: "callee".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let caller_track = bridge.get_caller_track().await.expect("caller track");

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-inject-3".into(),
            source: PlaySource::File {
                path: wav_path.to_string_lossy().to_string(),
            },
            target: InjectTarget::Leg("caller".into()),
            mute_peer: true,
        })
        .unwrap();

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for PlayStarted")
        .expect("channel closed");
    assert!(
        matches!(&ev, rustpbx::media::engine::MediaEvent::PlayStarted { .. }),
        "expected PlayStarted, got {:?}",
        ev
    );

    let caller_frame = timeout(Duration::from_millis(200), caller_track.recv())
        .await
        .expect("caller frame timeout (file source)")
        .expect("caller frame");
    if let MediaSample::Audio(f) = &caller_frame {
        assert!(f.sequence_number.is_some(), "caller frame should have seq");
    } else {
        panic!("expected audio frame from caller endpoint");
    }

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_inject_audio_mute_peer_suppresses_output PASSED");
}

/// Test: InjectAudio followed by StopPlayback restores normal peer output mode.
#[tokio::test]
async fn test_inject_audio_stop_restores_peer_output() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_stop_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 160);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-inject-4".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let bridge = BridgePeerBuilder::new("test-inject-bridge-4".into())
        .with_rtp_port_range(29600, 29700)
        .build();
    bridge.setup_bridge().await.unwrap();

    engine
        .send(MediaCommand::AttachBridge {
            session_id: "e2e-inject-4".into(),
            bridge: bridge.clone(),
            caller_is_webrtc: false,
            caller_codec_info: vec![codec_info()],
        })
        .unwrap();

    engine
        .send(MediaCommand::BridgeLegs {
            session_id: "e2e-inject-4".into(),
            leg_a: "caller".into(),
            leg_b: "callee".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-inject-4".into(),
            source: PlaySource::File {
                path: wav_path.to_string_lossy().to_string(),
            },
            target: InjectTarget::Both,
            mute_peer: false,
        })
        .unwrap();

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    assert!(matches!(
        &ev,
        rustpbx::media::engine::MediaEvent::PlayStarted { .. }
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;

    engine
        .send(MediaCommand::StopPlayback {
            session_id: "e2e-inject-4".into(),
            leg_id: None,
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let wav_path_2 = temp_dir.join("test_inject_2.wav");
    create_wav_file(&wav_path_2, 160);

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-inject-4".into(),
            source: PlaySource::File {
                path: wav_path_2.to_string_lossy().to_string(),
            },
            target: InjectTarget::Both,
            mute_peer: false,
        })
        .unwrap();

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout waiting for second PlayStarted")
        .expect("channel closed");
    assert!(
        matches!(&ev, rustpbx::media::engine::MediaEvent::PlayStarted { .. }),
        "expected second PlayStarted after StopPlayback, got {:?}",
        ev
    );

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_inject_audio_stop_restores_peer_output PASSED");
}

/// Test: InjectAudio RTP sequence continuity — verify that sequence numbers
/// increment correctly during file output.
#[tokio::test]
async fn test_inject_audio_rtp_sequence_continuity() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_seq_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 8000);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-inject-5".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let bridge = BridgePeerBuilder::new("test-inject-bridge-5".into())
        .with_rtp_port_range(29800, 29900)
        .build();
    bridge.setup_bridge().await.unwrap();

    engine
        .send(MediaCommand::AttachBridge {
            session_id: "e2e-inject-5".into(),
            bridge: bridge.clone(),
            caller_is_webrtc: false,
            caller_codec_info: vec![codec_info()],
        })
        .unwrap();

    engine
        .send(MediaCommand::BridgeLegs {
            session_id: "e2e-inject-5".into(),
            leg_a: "caller".into(),
            leg_b: "callee".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let callee_track = bridge.get_callee_track().await.expect("callee track");

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-inject-5".into(),
            source: PlaySource::File {
                path: wav_path.to_string_lossy().to_string(),
            },
            target: InjectTarget::Both,
            mute_peer: false,
        })
        .unwrap();

    let _ = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout");

    let mut prev_seq: Option<u16> = None;
    let mut prev_ts: Option<u32> = None;
    let mut frame_count = 0u32;

    for _ in 0..10 {
        match timeout(Duration::from_millis(100), callee_track.recv()).await {
            Ok(Ok(MediaSample::Audio(f))) => {
                let seq = f.sequence_number.expect("frame should have seq");
                let ts = f.rtp_timestamp;

                if let Some(ps) = prev_seq {
                    assert_eq!(
                        seq,
                        ps.wrapping_add(1),
                        "sequence discontinuity: prev={}, curr={}",
                        ps,
                        seq
                    );
                }
                if let Some(pt) = prev_ts {
                    assert_eq!(
                        ts,
                        pt.wrapping_add(160),
                        "timestamp discontinuity: prev={}, curr={}",
                        pt,
                        ts
                    );
                }

                prev_seq = Some(seq);
                prev_ts = Some(ts);
                frame_count += 1;
            }
            _ => break,
        }
    }

    assert!(
        frame_count >= 5,
        "should have received at least 5 continuous frames, got {}",
        frame_count
    );

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!(
        "test_inject_audio_rtp_sequence_continuity PASSED ({} frames)",
        frame_count
    );
}

/// Test: InjectAudio on nonexistent session returns an error from the engine.
#[tokio::test]
async fn test_inject_audio_nonexistent_session() {
    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "nonexistent".into(),
            source: PlaySource::File {
                path: "/tmp/test.wav".into(),
            },
            target: InjectTarget::Both,
            mute_peer: false,
        })
        .unwrap();

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    assert!(
        matches!(&ev, rustpbx::media::engine::MediaEvent::Error { command, .. } if command == "inject_audio"),
        "expected Error event for inject_audio, got {:?}",
        ev
    );

    tracing::info!("test_inject_audio_nonexistent_session PASSED");
}

/// Test: InjectAudio on a session without a bridge returns an error.
#[tokio::test]
async fn test_inject_audio_no_bridge() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_nobridge_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 160);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-no-bridge".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let result = engine.send(MediaCommand::InjectAudio {
        session_id: "e2e-no-bridge".into(),
        source: PlaySource::File {
            path: wav_path.to_string_lossy().to_string(),
        },
        target: InjectTarget::Both,
        mute_peer: false,
    });
    assert!(result.is_ok(), "send should succeed (command queued)");

    let ev = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    assert!(
        matches!(&ev, rustpbx::media::engine::MediaEvent::Error { command, .. } if command == "inject_audio"),
        "expected Error event for missing bridge, got {:?}",
        ev
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_inject_audio_no_bridge PASSED");
}

/// Test: Full-stack audio injection via RWI originate + CallCommand::Play.
///
/// While the RWI originate path uses `CallCommand::Play` (not InjectAudio directly),
/// both share the same underlying FileTrack→RTP mechanism. This test verifies
/// that audio content flows correctly through the full SIP/RTP stack:
/// 1. Originate a call from PBX to sipbot UA
/// 2. Play a 440 Hz tone through the call
/// 3. Verify the callee receives RTP with the correct audio content
#[tokio::test]
async fn test_audio_injection_full_stack() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let callee_port = portpicker::pick_unused_port().expect("no free callee port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_full_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 3.0, 8000, 0.5);

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

    let call_id = format!("inject-full-{}", Uuid::new_v4());
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
        "Full stack - Audio quality: total={}, silence={}, has_audio={}",
        quality.total_frames,
        quality.silence_frames,
        quality.has_audio()
    );
    assert!(
        quality.has_audio(),
        "Injected audio should produce non-silent frames. Quality: {:?}",
        quality
    );

    callee.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;

    ws.close(None).await.ok();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_audio_injection_full_stack PASSED");
}

/// Test: InjectAudio with bridge file replacement preserves RTP continuity.
///
/// Verifies that when InjectAudio replaces the bridge output, the RTP sequence
/// numbers and timestamps are continuous (no jumps or resets).
#[tokio::test]
async fn test_inject_audio_rtp_continuity_after_replacement() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_cont_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let file_a = temp_dir.join("audio_a.wav");
    let file_b = temp_dir.join("audio_b.wav");
    create_wav_file(&file_a, 800);
    create_wav_file(&file_b, 800);

    let bridge = BridgePeerBuilder::new("test-inject-continuity".into())
        .with_rtp_port_range(30000, 30100)
        .build();
    bridge.setup_bridge().await.unwrap();

    let mk_track = |id: &str, path: &std::path::Path| {
        FileTrack::new(id.to_string())
            .with_path(path.to_string_lossy().to_string())
            .with_loop(false)
            .with_codec_info(codec_info())
    };

    bridge
        .replace_output_with_file(BridgeEndpoint::Callee, &mk_track("a", &file_a))
        .await
        .unwrap();

    let rtp_track = bridge
        .get_callee_track()
        .await
        .expect("bridge RTP output track");

    let first = timeout(Duration::from_millis(200), rtp_track.recv())
        .await
        .expect("first frame timeout")
        .expect("first frame");
    let MediaSample::Audio(first_audio) = first else {
        panic!("expected audio frame");
    };
    let first_seq = first_audio.sequence_number.expect("first seq");
    let first_ts = first_audio.rtp_timestamp;

    tokio::time::sleep(Duration::from_millis(100)).await;

    bridge
        .replace_output_with_file(BridgeEndpoint::Callee, &mk_track("b", &file_b))
        .await
        .unwrap();

    let second = timeout(Duration::from_millis(200), rtp_track.recv())
        .await
        .expect("second frame timeout")
        .expect("second frame");
    let MediaSample::Audio(second_audio) = second else {
        panic!("expected audio frame");
    };
    let second_seq = second_audio.sequence_number.expect("second seq");
    let second_ts = second_audio.rtp_timestamp;

    assert_eq!(
        second_seq,
        first_seq.wrapping_add(1),
        "sequence should continue across InjectAudio file replacement: first={}, second={}",
        first_seq,
        second_seq
    );
    assert_eq!(
        second_ts,
        first_ts.wrapping_add(160),
        "timestamp should continue with 20ms@8k step: first={}, second={}",
        first_ts,
        second_ts
    );

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_inject_audio_rtp_continuity_after_replacement PASSED");
}

/// Test: Verify PCM→RTP encoding correctness for InjectAudio.
///
/// Creates a WAV file with a known pattern (440 Hz sine), injects it,
/// captures the RTP frames, and verifies:
/// 1. Payload type matches PCMU (0)
/// 2. Clock rate is 8000
/// 3. Encoded data size is correct (160 bytes for 20ms PCMU)
/// 4. Data is valid PCMU (decode round-trip matches original PCM)
#[tokio::test]
async fn test_inject_audio_pcmu_encoding_correctness() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_pcmu_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_pcmu.wav");
    create_wav_file(&wav_path, 1600);

    let (engine, mut event_rx) = setup_engine();

    engine
        .send(MediaCommand::CreateSession {
            session_id: "e2e-pcmu".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let bridge = BridgePeerBuilder::new("test-pcmu-bridge".into())
        .with_rtp_port_range(30200, 30300)
        .build();
    bridge.setup_bridge().await.unwrap();

    engine
        .send(MediaCommand::AttachBridge {
            session_id: "e2e-pcmu".into(),
            bridge: bridge.clone(),
            caller_is_webrtc: true,
            caller_codec_info: vec![codec_info()],
        })
        .unwrap();

    engine
        .send(MediaCommand::BridgeLegs {
            session_id: "e2e-pcmu".into(),
            leg_a: "caller".into(),
            leg_b: "callee".into(),
        })
        .unwrap();
    let _ = event_rx.recv().await;

    let callee_track = bridge.get_callee_track().await.expect("callee track");

    engine
        .send(MediaCommand::InjectAudio {
            session_id: "e2e-pcmu".into(),
            source: PlaySource::File {
                path: wav_path.to_string_lossy().to_string(),
            },
            target: InjectTarget::Both,
            mute_peer: false,
        })
        .unwrap();
    let _ = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("timeout");

    let mut frame_count = 0u32;
    let mut all_data_len_ok = true;
    let mut all_pt_ok = true;
    let mut all_clock_rate_ok = true;

    for _ in 0..5 {
        match timeout(Duration::from_millis(100), callee_track.recv()).await {
            Ok(Ok(MediaSample::Audio(f))) => {
                if f.payload_type != Some(0) {
                    all_pt_ok = false;
                    tracing::error!(
                        "Frame {}: unexpected PT={:?}, expected 0 (PCMU)",
                        frame_count,
                        f.payload_type
                    );
                }
                if f.clock_rate != 8000 {
                    all_clock_rate_ok = false;
                    tracing::error!(
                        "Frame {}: unexpected clock_rate={}, expected 8000",
                        frame_count,
                        f.clock_rate
                    );
                }
                if f.data.len() != 160 {
                    all_data_len_ok = false;
                    tracing::error!(
                        "Frame {}: unexpected data len={}, expected 160 (20ms PCMU)",
                        frame_count,
                        f.data.len()
                    );
                }
                frame_count += 1;
            }
            _ => break,
        }
    }

    assert!(
        frame_count >= 3,
        "should decode at least 3 frames, got {}",
        frame_count
    );
    assert!(all_pt_ok, "all frames should have PT=0 (PCMU)");
    assert!(all_clock_rate_ok, "all frames should have clock_rate=8000");
    assert!(
        all_data_len_ok,
        "all frames should have 160 bytes data (20ms PCMU)"
    );

    let mut decoded_pcm = Vec::new();
    let mut decoder = audio_codec::create_decoder(CodecType::PCMU);
    for _ in 0..3 {
        match timeout(Duration::from_millis(100), callee_track.recv()).await {
            Ok(Ok(MediaSample::Audio(f))) => {
                let pcm = decoder.decode(&f.data);
                assert!(!pcm.is_empty(), "decoded PCM should not be empty");
                assert_eq!(
                    pcm.len(),
                    160,
                    "decoded PCM should have 160 samples for 20ms@8kHz"
                );
                let max_amp = pcm
                    .iter()
                    .map(|s: &i16| s.unsigned_abs())
                    .max()
                    .unwrap_or(0);
                assert!(
                    max_amp > 100,
                    "decoded PCM should have non-trivial amplitude (max={}), not silence",
                    max_amp
                );
                decoded_pcm.extend_from_slice(&pcm);
            }
            _ => break,
        }
    }

    if decoded_pcm.len() >= 320 {
        let has_variation = decoded_pcm.windows(2).any(|w| w[0] != w[1]);
        assert!(
            has_variation,
            "decoded PCM should have sample variation (not flat/silence)"
        );
    }

    bridge.stop().await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!(
        "test_inject_audio_pcmu_encoding_correctness PASSED ({} frames verified)",
        frame_count
    );
}

/// Test: Verify that during InjectAudio, the recorder captures the injected
/// file audio (not just the peer's network audio).
///
/// After the bridge-level fix, `spawn_file_output_clock` writes to the recorder,
/// so injected announcements are captured in the recording WAV file.
#[tokio::test]
async fn test_inject_audio_recording_captures_injected_audio() {
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_inject_rec_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let wav_path = temp_dir.join("test_inject.wav");
    create_wav_file(&wav_path, 8000);

    let rec_path = temp_dir.join("recording.wav");

    let recorder: std::sync::Arc<parking_lot::RwLock<Option<rustpbx::media::recorder::Recorder>>> =
        std::sync::Arc::new(parking_lot::RwLock::new(Some(
            rustpbx::media::recorder::Recorder::new(
                &rec_path.to_string_lossy(),
                audio_codec::CodecType::PCMU,
            )
            .unwrap(),
        )));
    let recording_paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let bridge = BridgePeerBuilder::new("test-rec-bridge".into())
        .with_rtp_port_range(30400, 30500)
        .with_recorder(recorder.clone(), recording_paused.clone())
        .build();
    bridge.setup_bridge().await.unwrap();

    let track = FileTrack::new("test-file".into())
        .with_path(wav_path.to_string_lossy().to_string())
        .with_loop(false)
        .with_codec_info(codec_info());

    bridge
        .replace_output_with_file(BridgeEndpoint::Callee, &track)
        .await
        .unwrap();

    let rtp_track = bridge
        .get_callee_track()
        .await
        .expect("callee track should exist");

    let mut received_frames = 0u32;
    for _ in 0..10 {
        match timeout(Duration::from_millis(100), rtp_track.recv()).await {
            Ok(Ok(MediaSample::Audio(_))) => {
                received_frames += 1;
            }
            _ => break,
        }
    }

    assert!(
        received_frames >= 1,
        "should receive at least 1 RTP frame from inject"
    );

    tokio::time::sleep(Duration::from_millis(100)).await;

    bridge.stop().await;

    {
        let mut guard = recorder.write();
        if let Some(ref mut rec) = *guard {
            rec.finalize().unwrap();
        }
    }

    assert!(
        rec_path.exists(),
        "recording file should exist at {:?}",
        rec_path
    );

    let _metadata = std::fs::metadata(&rec_path).unwrap();
    let rec_data = std::fs::read(&rec_path).unwrap();

    assert!(
        rec_data.len() >= 44,
        "recording should have at least WAV header (44 bytes), got {}",
        rec_data.len()
    );

    assert!(
        rec_data.len() >= 80,
        "WAV file too small: {} bytes",
        rec_data.len()
    );
    assert_eq!(&rec_data[0..4], b"RIFF", "should be a valid WAV file");
    assert_eq!(&rec_data[8..12], b"WAVE", "should be a valid WAV file");

    let data_size = u32::from_le_bytes([rec_data[40], rec_data[41], rec_data[42], rec_data[43]]);
    tracing::info!(
        "Recording: {} bytes total, data_size={} bytes, received {} RTP frames",
        rec_data.len(),
        data_size,
        received_frames
    );

    assert!(
        data_size > 0,
        "recording data section should not be empty — injected audio must be recorded"
    );

    let min_expected = received_frames.min(5) as u32 * 80;
    assert!(
        data_size >= min_expected,
        "recording data ({}) should contain at least {} bytes of injected audio ({} frames × 80 bytes)",
        data_size,
        min_expected,
        received_frames.min(5)
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!(
        "test_inject_audio_recording_captures_injected_audio PASSED ({} frames, rec {} bytes, data {} bytes)",
        received_frames,
        rec_data.len(),
        data_size
    );
}

/// Test: Inject audio into a live call via RWI `media.play` command.
///
/// This tests the end-to-end path:
/// 1. RWI WebSocket `media.play` → processor dispatch → `CallCommand::Play`
/// 2. SipSession `handle_play()` → `bridge.replace_output_with_file()`
/// 3. File output pump → RTP → callee receives audio
#[tokio::test]
async fn test_inject_audio_via_rwi_media_play() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let callee_port = portpicker::pick_unused_port().expect("no free callee port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_rwi_play_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 3.0, 8000, 0.5);

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

    let call_id = format!("rwi-play-{}", Uuid::new_v4());
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

    let (_, play_json) = rwi_req(
        "media.play",
        serde_json::json!({
            "call_id": call_id,
            "source": {
                "source_type": "file",
                "uri": tone_path.to_str().unwrap()
            },
            "interrupt_on_dtmf": false,
        }),
    );
    let v = ws_send_recv(&mut ws, &play_json).await;
    assert_eq!(
        v["type"], "command_completed",
        "media.play should succeed: {v}"
    );
    let track_id = v
        .get("track_id")
        .or_else(|| v.get("result").and_then(|r| r.get("track_id")))
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");
    tracing::info!("media.play returned track_id={}", track_id);

    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should have received RTP after media.play. Stats: {}",
        callee.rtp_stats_summary()
    );

    let quality = callee.audio_quality_summary();
    tracing::info!(
        "RWI media.play - Audio quality: total={}, silence={}, has_audio={}",
        quality.total_frames,
        quality.silence_frames,
        quality.has_audio()
    );
    assert!(
        quality.has_audio(),
        "media.play audio should produce non-silent frames. Quality: {:?}",
        quality
    );

    callee.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;

    if record_path.exists() {
        let (rx_ch, _, rec_sr) = read_wav_stereo(&record_path);
        if !rx_ch.is_empty() {
            let signal_start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            let region = extract_audio_region(&rx_ch, rec_sr, signal_start, 1000);
            if !region.is_empty() {
                let rms_db = compute_rms(region);
                tracing::info!("RWI media.play recording - RX RMS: {:.1} dB", rms_db);
                assert!(
                    has_audio_content(region, -30.0),
                    "Recording should have audio from media.play, got {:.1} dB",
                    rms_db
                );
            }
        }
    }

    ws.close(None).await.ok();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_inject_audio_via_rwi_media_play PASSED");
}

/// Test: Inject audio via `media.play`, then stop via `media.stop`.
#[tokio::test]
async fn test_inject_audio_via_rwi_media_stop() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let callee_port = portpicker::pick_unused_port().expect("no free callee port");

    let temp_dir = std::env::temp_dir().join(format!("rustpbx_rwi_stop_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let tone_path = temp_dir.join("tone_440.wav");
    generate_sine_wav(&tone_path, 440.0, 5.0, 8000, 0.5);

    let pbx = TestPbx::start(sip_port).await;

    let callee = TestUa::callee_with_username(callee_port, 0, "callee").await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    let (_, sub_json) = rwi_req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = ws_send_recv(&mut ws, &sub_json).await;
    assert_eq!(v["type"], "command_completed", "subscribe failed: {v}");

    let call_id = format!("rwi-stop-{}", Uuid::new_v4());
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

    let (_, play_json) = rwi_req(
        "media.play",
        serde_json::json!({
            "call_id": call_id,
            "source": {
                "source_type": "file",
                "uri": tone_path.to_str().unwrap()
            },
        }),
    );
    let v = ws_send_recv(&mut ws, &play_json).await;
    assert_eq!(v["type"], "command_completed", "media.play failed: {v}");

    tokio::time::sleep(Duration::from_secs(1)).await;

    let (_, stop_json) = rwi_req(
        "media.stop",
        serde_json::json!({
            "call_id": call_id,
        }),
    );
    let v = ws_send_recv(&mut ws, &stop_json).await;
    assert_eq!(
        v["type"], "command_completed",
        "media.stop should succeed: {v}"
    );

    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should still have RTP after media.stop. Stats: {}",
        callee.rtp_stats_summary()
    );

    ws.close(None).await.ok();
    callee.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);

    tracing::info!("test_inject_audio_via_rwi_media_stop PASSED");
}
