//! P2P bidirectional audio e2e (sipbot caller + echo callee)
//!
//! Verifies a normal P2P call carries real, non-silent audio in BOTH
//! directions — not just RTP packet counts:
//!   sipbot(caller, plays a 440 Hz sine WAV looped)
//!     → PBX → sipbot(callee, echo)
//!     → callee RX = caller's sine audio (non-silent)
//!     → caller RX = callee's echo of that audio (non-silent)
//!
//! Usage: cargo test --test p2p_audio_e2e_test -- --nocapture

mod helpers;

use helpers::audio_verifier::generate_sine_wav;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::TestPbx;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Assert `has_rtp_rx` and, if available, non-silent audio on a UA.
async fn wait_for_audio(ua: &TestUa, label: &str, max_secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(max_secs);
    loop {
        if ua.has_rtp_rx() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label}: no RTP RX after {max_secs}s — {}",
            ua.rtp_stats_summary()
        );
        sleep(Duration::from_millis(200)).await;
    }
    // Let the audio analyzer accumulate frames before judging silence.
    sleep(Duration::from_secs(1)).await;
    let q = ua.audio_quality_summary();
    assert!(
        ua.has_rtp_rx(),
        "{label}: expected RX RTP — {}",
        ua.rtp_stats_summary()
    );
    assert!(
        q.has_audio(),
        "{label}: expected non-silent audio (total={}, silence={})",
        q.total_frames,
        q.silence_frames
    );
}

#[tokio::test]
async fn test_p2p_bidirectional_audio() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().expect("no SIP port");
    let caller_port = portpicker::pick_unused_port().expect("no caller port");
    let callee_port = portpicker::pick_unused_port().expect("no callee port");

    // Generate a short sine WAV (440 Hz, 0.5 s) the caller loops.
    let temp_dir = std::env::temp_dir().join(format!("p2p_audio_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).expect("temp dir");
    let sine_path: PathBuf = temp_dir.join("sine.wav");
    generate_sine_wav(&sine_path, 440.0, 0.5, 8000, 0.5);
    let sine_path_str = sine_path.to_string_lossy().to_string();

    // PBX (default config → media anchored, blind_transfer_use_refer=false).
    let pbx = TestPbx::start(sip_port).await;
    println!("[P2P] PBX up: sip={}, rwi={}", sip_port, pbx.rwi_url);

    // Callee: ring 1s, answer with echo.
    let callee = TestUa::callee(callee_port, 1).await;

    // Caller: dial callee, play sine WAV looped, stay up 15s.
    let target = format!("sip:callee@127.0.0.1:{}", callee_port);
    let caller = TestUa::caller_with_play(
        caller_port,
        "caller",
        target.clone(),
        sine_path_str,
        Some(15),
    )
    .await;
    println!("[P2P] Caller dialing {}, playing sine loop", target);

    // Wait for both legs to have real RTP + non-silent audio.
    wait_for_audio(&caller, "caller RX (callee echo)", 15).await;
    wait_for_audio(&callee, "callee RX (caller sine)", 15).await;

    println!(
        "[P2P] PASSED: caller={} callee={}",
        caller.rtp_stats_summary(),
        callee.rtp_stats_summary()
    );

    caller.stop();
    callee.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
}
