//! E2E: a queue's *custom* hold audio (`[queue.hold].audio_file` in the queue
//! TOML / `RouteQueueConfig.hold`) must actually reach the caller — as real
//! audio content, not just any RTP — and keep playing past one file duration
//! (the app-level replay that implements looping).
//!
//! Flow: caller → route(queue=support) → queue answers immediately → no agent
//! ever answers (bogus target) → caller is held, receiving the custom hold
//! tone. The caller media endpoint is a combined send/receive socket (real-UA
//! style) so the server's latched return path lands where we listen.
//!
//! Anti-fake-e2e: packet counts alone passed even while silence was relayed
//! (Python-side twin: `regression/suites/queue/test_queue.py::test_queue_hold_music_audio`),
//! so the captured μ-law payloads are decoded to PCM and analyzed with a
//! Goertzel filter: the 600 Hz custom tone must dominate, in both the first
//! and the last second of the capture.

use anyhow::{Result, anyhow};
use rustpbx::call::user::SipUser;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::media::wav_reader::{SampleFormat, WavSpec, WavWriter};
use rustpbx::proxy::routing::{
    MatchConditions, RouteAction, RouteQueueConfig, RouteQueueHoldConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::common::audio_analysis::{dominant_frequency, mulaw_payload_to_samples, rms};
use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::{RtpEndpoint, extract_media_endpoint};
use crate::common::test_ua::{TestUa, TestUaConfig};

/// Frequency of the custom hold tone baked into the temp WAV (Hz). Chosen to
/// differ from common PBX default tones (425/440/480 Hz ringback) so a
/// fall-back-to-default bug is caught, not just a silent-leg bug.
const HOLD_TONE_HZ: f64 = 600.0;
/// Length of the custom hold file (ms). Looping must carry audible tone past
/// this point — the last capture window starts after it.
const HOLD_FILE_MS: u64 = 2000;
const SAMPLE_RATE: u32 = 8000;
/// One second of 8 kHz audio.
const WINDOW_SAMPLES: usize = 8000;
/// Downlink packets required before analysis: 200 × 20 ms = 4 s of audio,
/// i.e. two full durations of the 2 s file.
const MIN_PCMU_PACKETS: usize = 200;
/// Candidate tones the analysis discriminates between (Hz).
const CANDIDATE_HZ: [f64; 8] = [300.0, 350.0, 425.0, 440.0, 480.0, 600.0, 900.0, 1000.0];
/// Loudness floor distinguishing a real tone from silence/comfort noise.
const RMS_FLOOR: f64 = 500.0;
/// Tolerance for the Goertzel dominant-frequency match (Hz).
const FREQ_TOLERANCE_HZ: f64 = 50.0;

fn write_sine_wav(path: &Path, freq_hz: f64, duration_ms: u64) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let num_samples = SAMPLE_RATE as u64 * duration_ms / 1000;
    let mut writer = WavWriter::create(path, spec)?;
    for i in 0..num_samples {
        let t = i as f64 / SAMPLE_RATE as f64;
        let sample = (8000.0 * (2.0 * std::f64::consts::PI * freq_hz * t).sin()) as i16;
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

fn create_queue_hold_proxy_config(port: u16, hold_audio: &Path) -> ProxyConfig {
    let mut config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        // The queue anchors the caller leg's media (it plays the hold audio),
        // so force full media proxying like the other media-path e2e tests.
        media_proxy: MediaProxyMode::All,
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };

    // Bogus agent target: dialing always fails, so the call stays queued and
    // the hold audio plays for as long as we need to capture it.
    let queue_config = RouteQueueConfig {
        name: Some("support".to_string()),
        accept_immediately: true,
        hold: Some(RouteQueueHoldConfig {
            audio_file: Some(hold_audio.to_string_lossy().to_string()),
            loop_playback: true,
        }),
        strategy: RouteQueueStrategyConfig {
            targets: vec![RouteQueueTargetConfig {
                uri: "sip:nobody@127.0.0.1:1".to_string(),
                label: Some("never-answers".to_string()),
            }],
            wait_timeout_secs: Some(30),
            ..Default::default()
        },
        ..Default::default()
    };
    config.queues.insert("support".to_string(), queue_config);

    let route = RouteRule {
        name: "route_to_support".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    config.routes = Some(vec![route]);

    config
}

fn assert_window_is_hold_tone(samples: &[i16], label: &str) {
    let level = rms(samples);
    assert!(
        level > RMS_FLOOR,
        "{label}: window is silent (rms {level:.1} ≤ {RMS_FLOOR}); no hold audio reached the caller"
    );
    let (freq, _) = dominant_frequency(samples, SAMPLE_RATE, &CANDIDATE_HZ);
    assert!(
        (freq - HOLD_TONE_HZ).abs() <= FREQ_TOLERANCE_HZ,
        "{label}: dominant frequency {freq:.0} Hz != custom hold tone {HOLD_TONE_HZ:.0} Hz \
         (rms {level:.1}) — the configured [queue.hold].audio_file is not what the caller hears"
    );
}

#[tokio::test]
async fn test_queue_custom_hold_audio_reaches_caller_and_loops() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    // 1. Bake the custom hold audio: a pure 600 Hz sine, 2 s @ 8 kHz mono.
    let hold_audio = std::env::temp_dir().join("rustpbx_e2e_queue_hold_600hz.wav");
    write_sine_wav(&hold_audio, HOLD_TONE_HZ, HOLD_FILE_MS)?;

    let server = E2eTestServer::start_with_inject(
        create_queue_hold_proxy_config(
            portpicker::pick_unused_port().unwrap_or(15070),
            &hold_audio,
        ),
        E2eTestServerInject {
            bans: None,
            queue_enricher: None,
            users: vec![SipUser {
                id: 1,
                username: "caller".to_string(),
                password: Some("password".to_string()),
                enabled: true,
                realm: Some("127.0.0.1".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .await?;
    let proxy_addr = server.proxy_addr;

    // 2. Caller media endpoint: one socket for both directions, so the
    //    latched return path lands exactly where we capture.
    let media = RtpEndpoint::bind(0).await?;
    media.start_receiving();

    let mut caller = TestUa::new(TestUaConfig {
        webrtc: false,
        username: "caller".to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(26100),
        proxy_addr,
    });
    caller.start().await?;

    // 3. Call the queue — it answers immediately, then parks us on hold.
    let sdp_offer = format!(
        "v=0\r\n\
         o=caller 1 0 IN IP4 127.0.0.1\r\ns=caller\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        media.local_port()?
    );
    let dialog_id = caller
        .make_call("support", Some(sdp_offer))
        .await
        .expect("queue should answer the caller leg");

    // 4. Uplink PCMU silence toward the server media endpoint (from the
    //    answer SDP) opens the latched return path; the hold tone flows back
    //    on the same socket.
    let answer_sdp = caller
        .get_negotiated_answer_sdp(&dialog_id)
        .await
        .ok_or_else(|| anyhow!("no negotiated answer SDP on caller leg"))?;
    let media_target: SocketAddr = extract_media_endpoint(&answer_sdp)
        .ok_or_else(|| anyhow!("answer SDP has no media endpoint: {answer_sdp}"))?;
    media.start_sending_pcmu(media_target, 15);

    // 5. Wait for enough downlink audio (≥ 2 file durations) before analyzing.
    let deadline = Instant::now() + Duration::from_secs(15);
    while media.captured_pcmu_payloads().len() < MIN_PCMU_PACKETS {
        if Instant::now() >= deadline {
            panic!(
                "caller received only {} PCMU packets (need {MIN_PCMU_PACKETS}) within 15s; \
                 payload types seen: {:?} — hold audio never reached the caller",
                media.captured_pcmu_payloads().len(),
                media.captured_payload_types(),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    caller.hangup(&dialog_id).await?;
    media.stop();
    server.stop();

    // 6. Content analysis on the captured μ-law payloads.
    let samples: Vec<i16> = media
        .captured_pcmu_payloads()
        .iter()
        .flat_map(|p| mulaw_payload_to_samples(p))
        .collect();
    assert!(
        samples.len() >= 2 * WINDOW_SAMPLES,
        "not enough decoded audio: {} samples",
        samples.len()
    );

    // First second: the custom hold tone is what the caller hears.
    assert_window_is_hold_tone(&samples[..WINDOW_SAMPLES], "first second");
    // Last second starts after HOLD_FILE_MS: playback must have continued
    // past the end of the 2 s file → looping (app-level replay) is active.
    assert_window_is_hold_tone(&samples[samples.len() - WINDOW_SAMPLES..], "last second");

    let _ = std::fs::remove_file(&hold_audio);
    Ok(())
}
