use crate::{
    media::track::{
        tts::{TtsCommand, TtsTrack},
        Track,
    },
    synthesis::{SynthesisClient, SynthesisOption, SynthesisType},
    Samples,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::{stream, Stream};
use std::pin::Pin;
use tokio::{
    sync::{broadcast, mpsc},
    time::Duration,
};

// A mock synthesis client that returns a predefined audio sample
struct MockSynthesisClient;

#[async_trait]
impl SynthesisClient for MockSynthesisClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("mock".to_string())
    }
    async fn synthesize<'a>(
        &'a self,
        _text: &'a str,
        _option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'a>>> {
        // Generate 1 second of sine wave at 440Hz, 16kHz sample rate, but split into chunks
        let sample_rate = 16000;
        let frequency = 440.0; // A4 note
        let duration_seconds = 1.0;
        let num_samples = (sample_rate as f64 * duration_seconds) as usize;

        // Split into 4 chunks (250ms each)
        let chunk_size = num_samples / 4;
        let mut chunks = Vec::new();

        for chunk_idx in 0..4 {
            let start = chunk_idx * chunk_size;
            let end = start + chunk_size;

            // Generate PCM audio data for this chunk (16-bit)
            let mut chunk_data = Vec::with_capacity(chunk_size * 2);
            for i in start..end {
                let t = i as f64 / sample_rate as f64;
                let amplitude = 16384.0; // Half of 16-bit range (32768/2)
                let sample =
                    (amplitude * (2.0 * std::f64::consts::PI * frequency * t).sin()) as i16;

                // Convert to bytes (little endian)
                chunk_data.push((sample & 0xFF) as u8);
                chunk_data.push(((sample >> 8) & 0xFF) as u8);
            }

            chunks.push(Ok(chunk_data));
        }

        // Create a stream from the chunks
        let stream = stream::iter(chunks);
        Ok(Box::pin(stream))
    }
}

#[tokio::test]
async fn test_tts_track_basic() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track".to_string();
    let client = MockSynthesisClient;
    let tts_track = TtsTrack::new(track_id.clone(), command_rx, Box::new(client));

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(TtsCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    // Wait for at least one audio frame
    let timeout = Duration::from_secs(5);
    let result = tokio::time::timeout(timeout, packet_rx.recv()).await;

    // Assert that we received a frame
    assert!(result.is_ok(), "Timed out waiting for audio frame");
    let frame = result.unwrap();
    assert!(frame.is_some(), "Expected audio frame, got None");

    let frame = frame.unwrap();

    // Verify the frame properties
    assert_eq!(frame.track_id, track_id, "Track ID mismatch");

    // Check that we have PCM samples
    match &frame.samples {
        Samples::PCM { samples } => {
            assert!(!samples.is_empty(), "Expected non-empty samples");
            // Ensure we have some reasonable amount of samples
            assert!(samples.len() > 100, "Too few samples in the frame");
        }
        _ => panic!("Expected PCM samples"),
    }

    // Stop the track
    tts_track.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_tts_track_multiple_commands() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track-multiple".to_string();
    let client = MockSynthesisClient;
    let tts_track =
        TtsTrack::new(track_id.clone(), command_rx, Box::new(client)).with_cache_enabled(false); // Disable caching for this test

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send multiple TTS commands
    for i in 1..=3 {
        command_tx.send(TtsCommand {
            text: format!("Test speech synthesis {}", i),
            play_id: Some(format!("test-{}", i)),
            ..Default::default()
        })?;
    }

    // Collect frames for a short period
    let timeout = Duration::from_secs(5);
    let mut frames = Vec::new();

    loop {
        match tokio::time::timeout(timeout, packet_rx.recv()).await {
            Ok(Some(frame)) => {
                frames.push(frame);
                if frames.len() >= 10 {
                    break; // Collected enough frames
                }
            }
            _ => break, // Either timeout or channel closed
        }
    }

    // Verify that we received multiple frames
    assert!(!frames.is_empty(), "Expected at least one audio frame");

    // Check that all frames have the correct track ID
    for frame in &frames {
        assert_eq!(frame.track_id, track_id, "Track ID mismatch");

        // Ensure each frame has valid PCM samples
        match &frame.samples {
            Samples::PCM { samples } => {
                assert!(!samples.is_empty(), "Expected non-empty samples");
            }
            _ => panic!("Expected PCM samples"),
        }
    }

    // Stop the track
    tts_track.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_tts_track_configuration() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with custom configuration
    let track_id = "test-track-config".to_string();
    let client = MockSynthesisClient;
    let custom_sample_rate = 8000; // Use 8kHz instead of default 16kHz
    let custom_ptime = Duration::from_millis(10); // Use 10ms packet time

    let tts_track = TtsTrack::new(track_id.clone(), command_rx, Box::new(client))
        .with_sample_rate(custom_sample_rate)
        .with_ptime(custom_ptime);

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(TtsCommand {
        text: "Test with custom configuration".to_string(),
        speaker: Some("test-speaker".to_string()),
        play_id: Some("config-test".to_string()),
        ..Default::default()
    })?;

    // Wait for an audio frame
    let timeout = Duration::from_secs(5);
    let result = tokio::time::timeout(timeout, packet_rx.recv()).await;

    // Verify the frame
    assert!(result.is_ok(), "Timed out waiting for audio frame");
    let frame = result.unwrap();
    assert!(frame.is_some(), "Expected audio frame, got None");

    let frame = frame.unwrap();

    // Verify the sample rate matches our configuration
    assert_eq!(
        frame.sample_rate, custom_sample_rate,
        "Sample rate mismatch"
    );

    // Stop the track
    tts_track.stop().await?;

    Ok(())
}
