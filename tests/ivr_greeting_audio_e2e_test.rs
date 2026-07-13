//! IVR greeting audio delivery regression.
//!
//! A tree-mode IVR with a known 440 Hz greeting WAV. The caller dials in, the
//! IVR app answers via the app media_bridge and plays the greeting. We verify
//! the caller receives non-silent RTP (the greeting is actually played through
//! the app-bridge output path that VoipBridge's `stop_forwarding` /
//! `ensure_media_anchored` also exercise).
//!
//! Frequency-level (Goertzel) verification of the play mechanism itself is
//! covered by `audio_content_e2e_test::test_tone_delivery_via_originate` (same
//! underlying MediaEngine → app-bridge output path). This test additionally
//! guards the IVR app → play wiring (greeting file → caller RX). A best-effort
//! Goertzel check runs when the sipbot caller recording has samples.

mod helpers;

use helpers::audio_verifier::{compute_rms, extract_audio_region, find_dominant_frequency, find_signal_start, generate_sine_wav, has_audio_content, read_wav_stereo};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::config::{ProxyConfig, UserBackendConfig};
use rustpbx::proxy::routing::RouteRule;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

/// Caller dials a tree-mode IVR whose greeting is a 440 Hz sine WAV; verify the
/// caller receives non-silent audio (greeting delivered), plus best-effort
/// frequency check on the caller recording.
#[tokio::test]
async fn test_ivr_greeting_delivers_audio() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new("rustpbx=info").unwrap())
        .try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no free caller port");
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_ivr_greet_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Known greeting: 440 Hz, 30 s, PCMU-friendly amplitude.
    let greeting_path = temp_dir.join("greeting_440.wav");
    generate_sine_wav(&greeting_path, 440.0, 30.0, 8000, 0.3);

    let ivr_toml = format!(
        r#"[ivr]
name = "greeting-ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "{}"
timeout_ms = 30000
max_retries = 99
"#,
        greeting_path.display()
    );
    let ivr_path = temp_dir.join("ivr.toml");
    std::fs::write(&ivr_path, &ivr_toml).unwrap();

    let route_toml = format!(
        r#"
name = "greeting-ivr-route"
priority = 100
app = "ivr"
auto_answer = true

[match]
"to.user" = "ivr"

[app_params]
file = "{}"
"#,
        ivr_path.display()
    );
    let routes: Vec<RouteRule> = vec![toml::from_str(&route_toml).expect("route")];

    let proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        ensure_user: Some(false),
        user_backends: vec![UserBackendConfig::Memory {
            users: Some(vec![SipUser {
                id: 0,
                enabled: true,
                username: "ivr".to_string(),
                password: None,
                realm: None,
                allow_guest_calls: true,
                ..Default::default()
            }]),
        }],
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            proxy_config: Some(proxy_config),
            routes: Some(routes),
            ..Default::default()
        },
    )
    .await;

    let caller_record = temp_dir.join("caller.wav");
    let target = format!("sip:ivr@{}", pbx.sip_host());
    let caller = TestUa::caller_with_target_and_record(
        caller_port,
        "caller",
        target.clone(),
        caller_record.to_string_lossy().to_string(),
        vec!["pcmu".to_string()],
    )
    .await;
    info!(%target, "caller dialing IVR greeting route");

    // Wait for the IVR to answer + play the greeting → caller receives audio.
    let mut got_audio = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let q = caller.audio_quality_summary();
        if q.has_audio() {
            got_audio = true;
            info!(
                "IVR greeting delivered: caller total={} silence={} has_audio={}",
                q.total_frames, q.silence_frames, q.has_audio()
            );
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    assert!(got_audio, "caller should receive non-silent IVR greeting audio");
    assert!(caller.has_rtp_rx(), "caller should have RX RTP from IVR greeting");

    // Best-effort frequency check on the caller recording.
    caller.stop();
    sleep(Duration::from_millis(400)).await;
    if caller_record.exists() {
        let (rx_ch, _tx_ch, rec_sr) = read_wav_stereo(&caller_record);
        info!("caller recording: {} RX samples at {} Hz", rx_ch.len(), rec_sr);
        if !rx_ch.is_empty() {
            let start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            let region = extract_audio_region(&rx_ch, rec_sr, start, 1500);
            if !region.is_empty() && has_audio_content(&region, -40.0) {
                let (freq, _mag) = find_dominant_frequency(&region, rec_sr, 200.0, 800.0, 5.0);
                info!("IVR greeting dominant freq in caller RX: {:.0} Hz", freq);
                assert!(
                    (freq - 440.0).abs() < 60.0,
                    "greeting frequency should be ~440 Hz, got {freq:.0}"
                );
                info!("IVR greeting 440 Hz verified in caller recording");
            }
        } else {
            info!("caller recording has 0 RX samples (sipbot caller recording limitation); verified via audio_quality instead");
        }
    }

    let _ = compute_rms; // keep import used
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
    info!("test_ivr_greeting_delivers_audio PASSED");
}
