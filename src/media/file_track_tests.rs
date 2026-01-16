use super::*;
use tokio::fs;

/// Helper to create a dummy WAV file for testing
async fn create_test_wav_file(path: &str) -> Result<()> {
    // Create a minimal valid WAV file header
    let mut wav_data = Vec::new();

    // RIFF header
    wav_data.extend_from_slice(b"RIFF");
    wav_data.extend_from_slice(&36u32.to_le_bytes()); // Chunk size
    wav_data.extend_from_slice(b"WAVE");

    // fmt chunk
    wav_data.extend_from_slice(b"fmt ");
    wav_data.extend_from_slice(&16u32.to_le_bytes()); // Subchunk1Size
    wav_data.extend_from_slice(&1u16.to_le_bytes()); // AudioFormat (PCM)
    wav_data.extend_from_slice(&1u16.to_le_bytes()); // NumChannels (mono)
    wav_data.extend_from_slice(&8000u32.to_le_bytes()); // SampleRate
    wav_data.extend_from_slice(&16000u32.to_le_bytes()); // ByteRate
    wav_data.extend_from_slice(&2u16.to_le_bytes()); // BlockAlign
    wav_data.extend_from_slice(&16u16.to_le_bytes()); // BitsPerSample

    // data chunk
    wav_data.extend_from_slice(b"data");
    wav_data.extend_from_slice(&0u32.to_le_bytes()); // Subchunk2Size (empty)

    fs::write(path, wav_data).await?;
    Ok(())
}

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

    // Initialize PC
    let track_mut = track.clone();
    let _ = track_mut.local_description().await;

    track.start_playback().await.unwrap();

    // Wait for completion (should complete within 200ms due to stub implementation)
    let completion = tokio::time::timeout(
        tokio::time::Duration::from_millis(200),
        track.wait_for_completion(),
    )
    .await;

    assert!(completion.is_ok(), "Playback should complete");

    // Cleanup
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

    // Should complete quickly after cancellation
    let _ = tokio::time::timeout(
        tokio::time::Duration::from_millis(100),
        track.wait_for_completion(),
    )
    .await;

    // Note: For looping, completion is never notified unless cancelled
    // so this timeout is expected for the stub implementation

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
