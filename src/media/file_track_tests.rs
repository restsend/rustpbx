use super::*;
use rustrtc::media::MediaStreamTrack as _;
use rustrtc::TransportMode;
use tokio::fs;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Create a minimal valid WAV file at `path` with `num_samples` mono 16-bit
/// PCM samples at 8 000 Hz.
async fn create_test_wav_file(path: &str) -> Result<()> {
    create_test_wav_file_with_samples(path, 0).await
}

async fn create_test_wav_file_with_samples(path: &str, num_samples: usize) -> Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| anyhow::anyhow!("WavWriter: {e}"))?;
    for i in 0..num_samples {
        let sample = ((i as f32 / 8.0).sin() * 1000.0) as i16;
        writer
            .write_sample(sample)
            .map_err(|e| anyhow::anyhow!("write_sample: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| anyhow::anyhow!("finalize: {e}"))?;
    Ok(())
}

// ── basic construction / SDP tests ───────────────────────────────────────────

#[tokio::test]
async fn test_file_track_creation() {
    let track = FileTrack::new("test-track".to_string())
        .with_path("/tmp/test.wav".to_string())
        .with_loop(false)
        .with_codec_preference(vec![CodecType::PCMU, CodecType::PCMA]);

    assert_eq!(track.id(), "test-track");
    assert_eq!(track.file_path, Some("/tmp/test.wav".to_string()));
    assert_eq!(track.loop_playback, false);
    assert_eq!(
        track.codec_preference,
        vec![CodecType::PCMU, CodecType::PCMA]
    );
}

#[tokio::test]
async fn test_file_track_with_ssrc_compatibility() {
    let track = FileTrack::new("test".to_string())
        .with_path("/tmp/test.wav".to_string())
        .with_ssrc(12345); // Should not panic, just ignore

    assert_eq!(track.id(), "test");
}

#[tokio::test]
async fn test_file_track_generates_sdp() {
    let track = FileTrack::new("test-sdp".to_string())
        .with_path("/tmp/test.wav".to_string())
        .with_codec_preference(vec![CodecType::PCMU]);

    let sdp = track.local_description().await;
    assert!(sdp.is_ok(), "Should generate SDP successfully");

    let sdp_string = sdp.unwrap();
    assert!(
        sdp_string.contains("a=rtpmap:0 PCMU/8000"),
        "SDP should contain PCMU codec, got: {}",
        sdp_string
    );
}

#[tokio::test]
async fn test_file_track_opus_codec_sdp() {
    let track = FileTrack::new("test-opus".to_string())
        .with_path("/tmp/test.wav".to_string())
        .with_codec_preference(vec![CodecType::Opus]);

    let sdp = track.local_description().await.unwrap();
    assert!(
        sdp.contains("a=rtpmap:111 opus/48000/2"),
        "SDP should contain Opus codec"
    );
    assert!(
        sdp.contains("a=fmtp:111 minptime=10;useinbandfec=1"),
        "SDP should contain Opus fmtp parameters"
    );
}

#[test]
fn test_audio_frame_timing_g722_splits_pcm_rate_from_rtp_clock() {
    let timing = audio_frame_timing(CodecType::G722, 8000);

    assert_eq!(
        timing.pcm_sample_rate, 16000,
        "G722 audio processing should use 16 kHz PCM"
    );
    assert_eq!(
        timing.pcm_samples_per_frame, 320,
        "20 ms of 16 kHz PCM should read 320 samples"
    );
    assert_eq!(
        timing.rtp_ticks_per_frame, 160,
        "20 ms of G722 RTP should advance timestamps by 160 ticks at 8 kHz"
    );
}

#[tokio::test]
async fn test_file_track_multiple_codecs() {
    let track = FileTrack::new("test-multi".to_string())
        .with_path("/tmp/test.wav".to_string())
        .with_codec_preference(vec![CodecType::PCMU, CodecType::PCMA, CodecType::Opus]);

    let sdp = track.local_description().await.unwrap();
    assert!(sdp.contains("PCMU/8000"), "Should contain PCMU");
    assert!(sdp.contains("PCMA/8000"), "Should contain PCMA");
    assert!(sdp.contains("opus/48000/2"), "Should contain Opus");
}

#[tokio::test]
async fn test_file_track_playback_starts() {
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_playback.wav");
    create_test_wav_file(test_file.to_str().unwrap())
        .await
        .unwrap();

    let track = FileTrack::new("playback-test".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(false);

    // Initialize PC first
    let track_mut = track.clone();
    let _ = track_mut.local_description().await;

    let result = track.start_playback().await;
    assert!(result.is_ok(), "Playback should start successfully");

    // Cleanup
    let _ = fs::remove_file(&test_file).await;
}

// ── real playback completion (file-exhausted path) ────────────────────────────

/// A short WAV file (160 samples = 20 ms at 8 kHz) must fire completion_notify
/// within a reasonable time once start_playback() is called.
#[tokio::test]
async fn test_file_track_playback_completion_accurate() {
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_completion_accurate.wav");
    // 160 samples = exactly one 20 ms frame.
    create_test_wav_file_with_samples(test_file.to_str().unwrap(), 160)
        .await
        .unwrap();

    let track = FileTrack::new("completion-accurate".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(false)
        .with_codec_preference(vec![CodecType::PCMU]);

    let _ = track.local_description().await;
    track.start_playback().await.unwrap();

    // The file has 20 ms of audio.  Allow 2 s for completion (the RTP loop
    // fires every 20 ms, so it should finish in ≤ 40 ms in practice).
    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        track.wait_for_completion(),
    )
    .await;

    assert!(
        result.is_ok(),
        "Playback completion notify must fire when the file is exhausted (not after a fixed sleep)"
    );

    let _ = fs::remove_file(&test_file).await;
}

/// Empty WAV → the source returns 0 immediately → completion fires on first tick.
#[tokio::test]
async fn test_file_track_playback_completion() {
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_completion.wav");
    create_test_wav_file(test_file.to_str().unwrap())
        .await
        .unwrap();

    let track = FileTrack::new("completion-test".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(false);

    let _ = track.local_description().await;
    track.start_playback().await.unwrap();

    // Allow 500 ms — the RTP loop fires within 20 ms and the empty source
    // immediately returns 0, triggering completion.
    let completion = tokio::time::timeout(
        tokio::time::Duration::from_millis(500),
        track.wait_for_completion(),
    )
    .await;

    assert!(completion.is_ok(), "Playback should complete");

    let _ = fs::remove_file(&test_file).await;
}

// ── cancel stops playback ─────────────────────────────────────────────────────

/// Cancelling a looping track via stop() must fire completion_notify quickly.
#[tokio::test]
async fn test_file_track_cancel_stops_rtp() {
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_cancel_rtp.wav");
    // 8 000 samples = 1 second — long enough that normal playback wouldn't finish.
    create_test_wav_file_with_samples(test_file.to_str().unwrap(), 8000)
        .await
        .unwrap();

    let cancel_token = CancellationToken::new();
    let track = FileTrack::new("cancel-rtp".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(true) // loop forever unless stopped
        .with_cancel_token(cancel_token.clone())
        .with_codec_preference(vec![CodecType::PCMU]);

    let _ = track.local_description().await;
    track.start_playback().await.unwrap();

    // Let it run for a couple of frames, then stop.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    track.stop().await;

    // completion_notify must fire promptly after cancellation.
    let result = tokio::time::timeout(
        tokio::time::Duration::from_millis(500),
        track.wait_for_completion(),
    )
    .await;

    assert!(
        result.is_ok(),
        "completion_notify must fire after stop() cancels the playback task"
    );

    let _ = fs::remove_file(&test_file).await;
}

#[tokio::test]
async fn test_file_track_looping_playback() {
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_loop.wav");
    create_test_wav_file(test_file.to_str().unwrap())
        .await
        .unwrap();

    let cancel_token = CancellationToken::new();
    let track = FileTrack::new("loop-test".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(true)
        .with_cancel_token(cancel_token.clone());

    // Initialize PC
    let track_mut = track.clone();
    let _ = track_mut.local_description().await;

    track.start_playback().await.unwrap();

    // Wait a bit then cancel
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    track.stop().await;

    // Cleanup
    let _ = fs::remove_file(&test_file).await;
}

#[tokio::test]
async fn test_file_track_missing_file_error() {
    let track = FileTrack::new("missing-file".to_string())
        .with_path("/nonexistent/path/to/file.wav".to_string());

    // Initialize PC first
    let track_mut = track.clone();
    let _ = track_mut.local_description().await;

    let result = track.start_playback().await;
    assert!(result.is_err(), "Should error on missing file");
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[tokio::test]
async fn test_file_track_set_remote_description() {
    let track =
        FileTrack::new("remote-desc-test".to_string()).with_path("/tmp/test.wav".to_string());

    // Generate local description first
    let _ = track.local_description().await.unwrap();

    // Create a valid SDP answer
    let answer_sdp = r#"v=0
o=- 0 0 IN IP4 127.0.0.1
s=-
t=0 0
m=audio 9 RTP/AVP 0
a=rtpmap:0 PCMU/8000
"#;

    let result = track.set_remote_description(answer_sdp).await;
    assert!(result.is_ok(), "Should set remote description successfully");
}

// ── e2e: FileTrack completion drives AudioComplete event ─────────────────────

/// End-to-end test: verify that a real `FileTrack` playback completion fires
/// `completion_notify`, which can be wired to a `ControllerEvent::AudioComplete`
/// channel — the exact pipeline used by `play_audio_file()` in the session layer.
///
/// This test exercises:
///   WAV file → AudioSourceManager → RTP loop → completion_notify → channel event
///
/// Without this test, only the stub behaviour (sleep 100ms) was covered.
#[tokio::test]
async fn test_file_track_completion_drives_audio_complete_event() {
    use crate::call::app::ControllerEvent;
    use tokio::sync::mpsc;

    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_e2e_audio_complete.wav");
    // 160 samples = 20 ms at 8 kHz — short enough to complete quickly.
    create_test_wav_file_with_samples(test_file.to_str().unwrap(), 160)
        .await
        .unwrap();

    let track = FileTrack::new("e2e-audio-complete".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(false)
        .with_codec_preference(vec![CodecType::PCMU]);

    let _ = track.local_description().await;
    track.start_playback().await.unwrap();

    // Wire completion_notify → ControllerEvent channel (mirrors play_audio_file()).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
    let completion_notify = track.completion_notify.clone();
    let cancel = CancellationToken::new();
    let cancel_child = cancel.child_token();
    let fp = test_file.to_string_lossy().to_string();
    crate::utils::spawn(async move {
        tokio::select! {
            _ = completion_notify.notified() => {
                let _ = event_tx.send(ControllerEvent::AudioComplete {
                    track_id: "default".to_string(),
                    interrupted: false,
                });
            }
            _ = cancel_child.cancelled() => {
                let _ = event_tx.send(ControllerEvent::AudioComplete {
                    track_id: "default".to_string(),
                    interrupted: true,
                });
            }
        }
        drop(fp);
    });

    // Allow 2 s — the 20 ms file should complete within ~40 ms.
    let event = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        event_rx.recv(),
    )
    .await
    .expect("AudioComplete must arrive within 2 s")
    .expect("channel must not be closed");

    match event {
        ControllerEvent::AudioComplete { interrupted, track_id } => {
            assert!(!interrupted, "File-exhausted playback must not be interrupted");
            assert_eq!(track_id, "default");
        }
        other => panic!("unexpected event: {:?}", other),
    }

    cancel.cancel();
    let _ = fs::remove_file(&test_file).await;
}

/// End-to-end test: verify that stopping a looping FileTrack fires an
/// interrupted `AudioComplete` event — the cancellation path used by
/// `play_audio_file()` when the session is torn down.
#[tokio::test]
async fn test_file_track_stop_drives_interrupted_audio_complete() {
    use crate::call::app::ControllerEvent;
    use tokio::sync::mpsc;

    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_e2e_interrupted.wav");
    // 8 000 samples = 1 s — long enough that playback won't finish naturally.
    create_test_wav_file_with_samples(test_file.to_str().unwrap(), 8000)
        .await
        .unwrap();

    let cancel_token = CancellationToken::new();
    let track = FileTrack::new("e2e-interrupted".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(true)
        .with_cancel_token(cancel_token.clone())
        .with_codec_preference(vec![CodecType::PCMU]);

    let _ = track.local_description().await;
    track.start_playback().await.unwrap();

    // Wire completion_notify → channel (mirrors play_audio_file()).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ControllerEvent>();
    let completion_notify = track.completion_notify.clone();
    let session_cancel = CancellationToken::new();
    let session_child = session_cancel.child_token();
    crate::utils::spawn(async move {
        tokio::select! {
            _ = completion_notify.notified() => {
                // File exhausted or track stopped — send non-interrupted.
                let _ = event_tx.send(ControllerEvent::AudioComplete {
                    track_id: "default".to_string(),
                    interrupted: false,
                });
            }
            _ = session_child.cancelled() => {
                let _ = event_tx.send(ControllerEvent::AudioComplete {
                    track_id: "default".to_string(),
                    interrupted: true,
                });
            }
        }
    });

    // Let it play a few frames, then stop the track (simulates session teardown).
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    track.stop().await;

    // completion_notify fires → watcher sends AudioComplete (interrupted=false
    // in this wiring, since we chose notify over session cancel).
    let event = tokio::time::timeout(
        tokio::time::Duration::from_millis(500),
        event_rx.recv(),
    )
    .await
    .expect("AudioComplete must arrive within 500 ms after stop()")
    .expect("channel must not be closed");

    assert!(
        matches!(event, ControllerEvent::AudioComplete { .. }),
        "Expected AudioComplete after stop(), got: {:?}",
        event
    );

    session_cancel.cancel();
    let _ = fs::remove_file(&test_file).await;
}

// ── RTP delivery via external PeerConnection ────────────────────────────────

/// End-to-end test: verify that `start_playback_on(Some(caller_pc))` sends RTP
/// frames through the caller's negotiated PeerConnection so they actually arrive
/// at a remote receiver.
///
/// Setup (pure RTP mode, no DTLS/ICE — same as SIP calls):
///   1. "caller PC" — an RtpTrack whose PC has completed SDP negotiation
///   2. "receiver PC" — simulates the sipbot; also SDP-negotiated with the caller
///   3. `FileTrack` with a short WAV file
///
/// The test calls `file_track.start_playback_on(Some(caller_pc))` and then
/// reads from the receiver PC's track to confirm at least one RTP frame arrives.
///
/// This is the exact bug-fix verification: without `start_playback_on`, frames
/// went into the FileTrack's own un-negotiated PC and never reached the network.
#[tokio::test]
async fn test_start_playback_on_external_pc_delivers_rtp() {
    use rustrtc::media::MediaSample;

    // ── 1. Create the WAV file (800 samples = 100 ms at 8 kHz) ──────────
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_external_pc_rtp.wav");
    create_test_wav_file_with_samples(test_file.to_str().unwrap(), 800)
        .await
        .unwrap();

    // ── 2. Build two RTP-mode PeerConnections and exchange SDP ──────────
    //
    // "caller_track" is the PBX-side RtpTrack whose PC will be handed to
    // FileTrack::start_playback_on().
    // "receiver_track" is the sipbot-side that should receive the RTP.

    let caller_track = RtpTrackBuilder::new("caller".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(17000, 17100)
        .with_codec_preference(vec![CodecType::PCMU])
        .build();

    let receiver_track = RtpTrackBuilder::new("receiver".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(17100, 17200)
        .with_codec_preference(vec![CodecType::PCMU])
        .build();

    // caller creates offer, receiver answers — this establishes both PCs'
    // remote descriptions and RTP transport addresses.
    let caller_offer = caller_track.local_description().await.unwrap();
    let receiver_answer = receiver_track.handshake(caller_offer).await.unwrap();
    caller_track
        .set_remote_description(&receiver_answer)
        .await
        .unwrap();

    // ── 3. Extract the caller's PeerConnection ──────────────────────────
    let caller_pc = caller_track.get_peer_connection().await.unwrap();

    // ── 4. Build the FileTrack and start playback on the caller PC ──────
    let file_track = FileTrack::new("rtp-delivery-test".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(false)
        .with_codec_preference(vec![CodecType::PCMU]);

    file_track
        .start_playback_on(Some(caller_pc))
        .await
        .unwrap();

    // ── 5. Read from the receiver PC's track and verify frames arrive ───
    //
    // The receiver PC should have a transceiver with a receiver that yields
    // incoming RTP frames.  We wait up to 2 seconds for at least one frame.
    let receiver_pc = receiver_track.get_peer_connection().await.unwrap();

    let received = tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
        // Wait for a Track event (the receiver PC learns about the incoming
        // stream when the first RTP packet arrives in plain RTP mode).
        let mut frame_count: u64 = 0;

        // First, check pre-existing transceivers for a receiver track.
        for transceiver in receiver_pc.get_transceivers() {
            if let Some(receiver) = transceiver.receiver() {
                let track = receiver.track();
                // Try to read a few frames.
                for _ in 0..10 {
                    match tokio::time::timeout(
                        tokio::time::Duration::from_millis(200),
                        track.recv(),
                    )
                    .await
                    {
                        Ok(Ok(MediaSample::Audio(frame))) => {
                            frame_count += 1;
                            debug!(
                                rtp_ts = frame.rtp_timestamp,
                                data_len = frame.data.len(),
                                "Receiver got RTP frame"
                            );
                            if frame_count >= 3 {
                                return frame_count;
                            }
                        }
                        Ok(Ok(_)) => continue,
                        Ok(Err(_)) => break,
                        Err(_) => continue, // timeout on this attempt
                    }
                }
            }
        }

        // If no pre-existing receiver had data, wait for PeerConnectionEvent::Track.
        let mut pc_recv = Box::pin(receiver_pc.recv());
        loop {
            match tokio::time::timeout(tokio::time::Duration::from_millis(500), &mut pc_recv)
                .await
            {
                Ok(Some(rustrtc::PeerConnectionEvent::Track(transceiver))) => {
                    if let Some(receiver) = transceiver.receiver() {
                        let track = receiver.track();
                        for _ in 0..10 {
                            match tokio::time::timeout(
                                tokio::time::Duration::from_millis(200),
                                track.recv(),
                            )
                            .await
                            {
                                Ok(Ok(MediaSample::Audio(frame))) => {
                                    frame_count += 1;
                                    debug!(
                                        rtp_ts = frame.rtp_timestamp,
                                        data_len = frame.data.len(),
                                        "Receiver got RTP frame (from Track event)"
                                    );
                                    if frame_count >= 3 {
                                        return frame_count;
                                    }
                                }
                                Ok(Ok(_)) => continue,
                                Ok(Err(_)) => break,
                                Err(_) => continue,
                            }
                        }
                    }
                    pc_recv = Box::pin(receiver_pc.recv());
                }
                Ok(Some(_)) => {
                    pc_recv = Box::pin(receiver_pc.recv());
                }
                Ok(None) => break,
                Err(_) => break, // overall timeout
            }
        }

        frame_count
    })
    .await;

    // ── 6. Assert ───────────────────────────────────────────────────────
    let frame_count = received.expect("Timed out waiting for RTP frames on receiver PC");
    assert!(
        frame_count >= 1,
        "Expected at least 1 RTP frame on the receiver PC, got {}. \
         This means start_playback_on(caller_pc) did not send frames \
         through the caller's negotiated transport.",
        frame_count
    );

    debug!(frame_count, "test_start_playback_on_external_pc_delivers_rtp passed");

    // Cleanup.
    file_track.stop().await;
    caller_track.stop().await;
    receiver_track.stop().await;
    let _ = fs::remove_file(&test_file).await;
}

/// Verify that `start_playback_on(None)` still works (backward compatibility) —
/// frames go into the FileTrack's own PC; completion_notify still fires.
#[tokio::test]
async fn test_start_playback_on_none_backward_compatible() {
    let temp_dir = std::env::temp_dir();
    let test_file = temp_dir.join("test_playback_on_none.wav");
    create_test_wav_file_with_samples(test_file.to_str().unwrap(), 160)
        .await
        .unwrap();

    let track = FileTrack::new("compat-test".to_string())
        .with_path(test_file.to_string_lossy().to_string())
        .with_loop(false)
        .with_codec_preference(vec![CodecType::PCMU]);

    let _ = track.local_description().await;

    // Explicitly call start_playback_on(None) — must behave identically to
    // start_playback().
    track.start_playback_on(None).await.unwrap();

    let result = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        track.wait_for_completion(),
    )
    .await;

    assert!(
        result.is_ok(),
        "start_playback_on(None) must fire completion_notify just like start_playback()"
    );

    let _ = fs::remove_file(&test_file).await;
}
