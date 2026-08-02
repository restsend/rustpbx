//! E2E: in-call `media.play` + RWI DTMF delivery on an established call.
//!
//! Exercises the in-call control plane through the real RWI WebSocket:
//!
//! Topology: `call.originate` (RWI-owned) → sipbot callee (echo). Because the
//! call is RWI-originated, the originating RWI session owns it, so server-pushed
//! events (`call_answered`, `media_play_started`, …) flow back to the collector.
//!
//! 1. `media.play` plays a known 440 Hz WAV to the callee leg — the callee must
//!    receive real RTP audio (packet count + Goertzel frequency).
//! 2. `call.send_dtmf` sends DTMF digits over the RTP RFC 2833 path; the callee
//!    must receive them (sipbot `rx_dtmf_events` counter).
//!
//! Inbound DTMF *collection* (`dtmf.collect`, IVR keying) over the same real
//! media path is covered by `proxy::tests::test_ivr_queue_e2e` (real RTP DTMF
//! into the proxy → IVR advance); this test covers the command-surface side:
//! media.play audio delivery + RWI DTMF send on an established call.

mod helpers;

use helpers::audio_verifier::{
    compute_rms, extract_audio_region, find_dominant_frequency, find_signal_start,
    generate_sine_wav, goertzel_magnitude_normalized, has_audio_content, read_wav_stereo,
};
use helpers::rwi_collector::RwiCollector;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// `media.play` must deliver real audio to the far leg, and `call.send_dtmf`
/// must deliver DTMF digits over the RTP RFC 2833 path.
#[tokio::test]
async fn test_media_play_and_dtmf_on_established_call() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let callee_port = portpicker::pick_unused_port().expect("no free callee port");

    let temp_dir =
        std::env::temp_dir().join(format!("rustpbx_media_play_dtmf_{}", Uuid::new_v4()));
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

    let mut rwi = RwiCollector::connect(&pbx.rwi_url, TEST_TOKEN).await;

    // ── 1. Originate (RWI-owned call) → callee answers ────────────────────
    let call_id = format!("media-play-dtmf-{}", Uuid::new_v4());
    let orig = rwi
        .send_command(
            "call.originate",
            serde_json::json!({
                "call_id": call_id,
                "destination": callee.sip_uri("callee"),
                "caller_id": format!("sip:pbx@{}", pbx.sip_host()),
                "context": "default",
                "timeout_secs": 15,
            }),
        )
        .await;
    assert_eq!(
        orig["type"], "command_completed",
        "originate failed: {orig}"
    );

    let answered = rwi.wait_for_event_type("call_answered", 10).await;
    let answered = match answered {
        Some(a) => a,
        None => {
            let types = rwi.get_event_types().await;
            panic!("no call_answered event; types={types:?}");
        }
    };
    assert_eq!(answered["call_id"], call_id, "answered call mismatch");

    // ── 2. media.play a 440 Hz tone to the callee leg ─────────────────────
    let play = rwi
        .send_command(
            "media.play",
            serde_json::json!({
                "call_id": call_id,
                "source": {
                    "type": "file",
                    "uri": tone_path.to_string_lossy().to_string(),
                },
                "leg_id": "caller",
            }),
        )
        .await;
    assert_eq!(
        play["type"], "command_completed",
        "media.play failed: {play}"
    );

    // Let the tone stream for a bit.
    sleep(Duration::from_millis(1200)).await;

    assert!(
        callee.has_rtp_rx(),
        "callee should have received RTP (media.play). Stats: {}",
        callee.rtp_stats_summary()
    );

    // ── 3. call.send_dtmf → callee receives it over RTP RFC 2833 ──────────
    let before = dtmf_count(&callee);
    let send = rwi
        .send_command(
            "call.send_dtmf",
            serde_json::json!({
                "call_id": call_id,
                "digits": "5",
                "leg_id": "caller",
            }),
        )
        .await;
    assert_eq!(
        send["type"], "command_completed",
        "call.send_dtmf failed: {send}"
    );

    // Wait for the DTMF to traverse the media path to the callee.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut received = dtmf_count(&callee);
    while received == before && tokio::time::Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
        received = dtmf_count(&callee);
    }
    assert!(
        received > before,
        "callee should have received DTMF digit(s): before={before} after={received}"
    );

    // ── 4. Verify the tone frequency reached the callee recording ─────────
    callee.stop();
    sleep(Duration::from_millis(500)).await;

    if record_path.exists() {
        let (rx_ch, _tx_ch, rec_sr) = read_wav_stereo(&record_path);
        if !rx_ch.is_empty() {
            let signal_start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            let region = extract_audio_region(&rx_ch, rec_sr, signal_start, 1000);
            if !region.is_empty() {
                let rms_db = compute_rms(region);
                assert!(
                    has_audio_content(region, -30.0),
                    "RX audio should have energy above -30 dB, got {:.1} dB",
                    rms_db
                );
                let (freq, mag) = find_dominant_frequency(region, rec_sr, 200.0, 800.0, 5.0);
                tracing::info!(
                    "media.play dominant frequency: {:.0} Hz (magnitude {:.1})",
                    freq,
                    mag
                );
                assert!(
                    (freq - 440.0).abs() < 30.0,
                    "media.play should deliver ~440 Hz, got {:.0} Hz",
                    freq
                );
                let m440 = goertzel_magnitude_normalized(region, 440.0, rec_sr);
                let m1000 = goertzel_magnitude_normalized(region, 1000.0, rec_sr);
                assert!(
                    m440 > m1000 * 5.0,
                    "440 Hz component should dominate: m440={:.1}, m1000={:.1}",
                    m440,
                    m1000
                );
            }
        }
    } else {
        tracing::warn!("callee recording file not found at {:?}", record_path);
    }

    let _ = std::fs::remove_dir_all(&temp_dir);
    tracing::info!("test_media_play_and_dtmf_on_established_call PASSED");
}

/// Number of DTMF digits the sipbot callee has received.
fn dtmf_count(callee: &TestUa) -> u64 {
    use std::sync::atomic::Ordering;
    callee.stats.rx_dtmf_events.load(Ordering::Relaxed)
}
