use crate::{
    event::SessionEvent,
    media::Samples,
    media::track::{Track, tts::TtsTrack},
    synthesis::{
        Subtitle, SynthesisClient, SynthesisCommand, SynthesisEvent, SynthesisOption, SynthesisType,
    },
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::BufMut;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::time::Instant;
use tokio::{
    sync::{broadcast, mpsc},
    time::Duration,
};
use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream};
use tokio_util::sync::CancellationToken;
use tracing::debug;

// A mock synthesis client that supports both streaming and non-streaming modes
struct MockSynthesisClient {
    // Channel for sending events back to the stream
    event_sender: Option<mpsc::UnboundedSender<(Option<usize>, Result<SynthesisEvent>)>>,
    // Current mode (streaming vs non-streaming)
    streaming: bool,
}

impl MockSynthesisClient {
    fn new(streaming: bool) -> Self {
        Self {
            event_sender: None,
            streaming,
        }
    }

    // Generate a simple sine wave audio sample for testing
    fn generate_audio_sample(text: &str, sample_rate: u32) -> (Vec<u8>, u32) {
        let frequency = 440.0; // A4 note
        // Duration based on text length (roughly 100ms per character)
        let duration_seconds = (text.len() as f64 * 0.1).max(0.5).min(3.0);
        let num_samples = (sample_rate as f64 * duration_seconds) as usize;

        // Generate PCM audio data (16-bit)
        let mut audio_data = Vec::with_capacity(num_samples * 2);
        for i in 0..num_samples {
            let t = i as f64 / sample_rate as f64;
            let amplitude = 16384.0; // Half of 16-bit range (32768/2)
            let sample = (amplitude * (2.0 * std::f64::consts::PI * frequency * t).sin()) as i16;
            audio_data.put_i16_le(sample);
        }

        (audio_data, duration_seconds as u32)
    }

    // Generate subtitles for the given text
    fn generate_subtitles(text: &str, duration_ms: u32) -> Vec<Subtitle> {
        vec![Subtitle::new(
            text.to_string(),
            0,
            duration_ms,
            0,
            text.len() as u32,
        )]
    }

    // Send events to the stream
    async fn send_event(&self, cmd_seq: Option<usize>, event: SynthesisEvent) {
        if let Some(sender) = &self.event_sender {
            let _ = sender.send((cmd_seq, Ok(event)));
        }
    }
}

#[async_trait]
impl SynthesisClient for MockSynthesisClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("mock".to_string())
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.event_sender = Some(event_tx);
        Ok(UnboundedReceiverStream::new(event_rx).boxed())
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        let sample_rate = option
            .as_ref()
            .and_then(|opt| opt.samplerate)
            .unwrap_or(16000) as u32;

        // Generate audio data
        let (audio_data, duration_ms) = Self::generate_audio_sample(text, sample_rate);

        self.send_event(cmd_seq, SynthesisEvent::AudioChunk(Bytes::from(audio_data)))
            .await;
        self.send_event(
            cmd_seq,
            SynthesisEvent::Subtitles(Self::generate_subtitles(text, duration_ms)),
        )
        .await;
        if !self.streaming {
            self.send_event(cmd_seq, SynthesisEvent::Finished).await;
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(sender) = &self.event_sender {
            // sending finished for streaming mode
            let _ = sender.send((None, Ok(SynthesisEvent::Finished)));
        }
        self.event_sender.take();
        Ok(())
    }
}

#[tokio::test]
async fn test_tts_track_basic_non_streaming() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let non_streaming_client = MockSynthesisClient::new(false);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(non_streaming_client),
    );

    // Create channels for the test
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    // Wait for at least one audio frame
    let frame = packet_rx.recv().await.unwrap();
    // Verify the frame properties
    assert_eq!(frame.track_id, track_id, "Track ID mismatch");
    debug!("frame: {:?}", frame);
    // Check that we have PCM samples
    if let Samples::PCM { samples } = &frame.samples {
        assert!(samples.len() > 100, "Too few samples in the frame");
    } else {
        panic!("Expected PCM samples");
    }

    // Stop the track
    tts_track.stop().await?;
    let mut track_end = false;
    while let Ok(event) = event_rx.recv().await {
        if let SessionEvent::TrackEnd { .. } = event {
            track_end = true;
            break;
        }
    }
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_basic_streaming() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let streaming_client = MockSynthesisClient::new(true);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        true,
        None,
        command_rx,
        Box::new(streaming_client),
    );

    // Create channels for the test
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    // Wait for at least one audio frame
    let frame = packet_rx.recv().await.unwrap();
    // Verify the frame properties
    assert_eq!(frame.track_id, track_id, "Track ID mismatch");
    // Check that we have PCM samples
    if let Samples::PCM { samples } = &frame.samples {
        assert!(samples.len() > 100, "Too few samples in the frame");
    } else {
        panic!("Expected PCM samples");
    }

    // Stop the track
    tts_track.stop().await?;
    let mut track_end = false;
    while let Ok(event) = event_rx.recv().await {
        if let SessionEvent::TrackEnd { .. } = event {
            track_end = true;
            break;
        }
    }
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_multiple_commands_non_streaming() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track-multiple".to_string();
    let client = MockSynthesisClient::new(false);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_cache_enabled(false); // Disable caching for this test

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send multiple TTS commands
    for i in 1..=3 {
        command_tx.send(SynthesisCommand {
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
    let client = MockSynthesisClient::new(false);
    let custom_sample_rate = 8000; // Use 8kHz instead of default 16kHz
    let custom_ptime = Duration::from_millis(10); // Use 10ms packet time

    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_sample_rate(custom_sample_rate)
    .with_ptime(custom_ptime);

    // Create channels for the test
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
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

#[tokio::test]
async fn test_tts_track_interrupt() -> Result<()> {
    // Create a command channel
    let cancel_token = CancellationToken::new();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track".to_string();
    let client = MockSynthesisClient::new(false);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_cancel_token(cancel_token.clone());

    // Create channels for the test
    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, mut _packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    tokio::time::sleep(Duration::from_millis(50)).await;
    tts_track.stop().await?;
    // Wait for at least one audio frame
    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;
    let mut interrupted = false;
    let mut track_end = false;
    for result in results {
        match result {
            Ok(SessionEvent::Interruption { .. }) => {
                interrupted = true;
            }
            Ok(SessionEvent::TrackEnd { .. }) => {
                track_end = true;
            }
            _ => {}
        }
    }
    assert!(interrupted, "Track was not interrupted");
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_interrupt_graceful() -> Result<()> {
    // Create a command channel
    let cancel_token = CancellationToken::new();
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with our mock client
    let track_id = "test-track".to_string();
    let client = MockSynthesisClient::new(false);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(client),
    )
    .with_cancel_token(cancel_token.clone());

    // Create channels for the test
    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, mut _packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test speech synthesis".to_string(),
        ..Default::default()
    })?;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let now = Instant::now();
    tts_track.stop_graceful().await?;
    // Wait for at least one audio frame
    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;
    let mut interrupted = false;
    let mut track_end = false;
    for result in results {
        match result {
            Ok(SessionEvent::Interruption { .. }) => {
                interrupted = true;
            }
            Ok(SessionEvent::TrackEnd { .. }) => {
                track_end = true;
            }
            _ => {}
        }
    }
    assert!(
        now.elapsed() > Duration::from_millis(100),
        "Track was interrupted too early"
    );
    assert!(interrupted, "Track was not interrupted");
    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_end_of_stream() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let non_streaming_client = MockSynthesisClient::new(false);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(non_streaming_client),
    );

    // Create channels for the test
    let (event_tx, event_rx) = broadcast::channel(16);
    let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text: "Test".to_string(),
        end_of_stream: true,
        ..Default::default()
    })?;

    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    let results = BroadcastStream::new(event_rx)
        .take_until(timeout)
        .collect::<Vec<_>>()
        .await;

    let mut track_end = false;
    for result in results {
        match result {
            Ok(SessionEvent::TrackEnd { .. }) => {
                track_end = true;
            }
            _ => {}
        }
    }

    assert!(track_end, "Track end event not received");
    Ok(())
}

#[tokio::test]
async fn test_tts_track_base64() -> Result<()> {
    // Create a command channel
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    // Create a TtsTrack with non-streaming mode
    let track_id = "test-track".to_string();
    let non_streaming_client = MockSynthesisClient::new(false);
    let tts_track = TtsTrack::new(
        track_id.clone(),
        "test_session".to_string(),
        false,
        None,
        command_rx,
        Box::new(non_streaming_client),
    );

    // Create channels for the test
    let (event_tx, mut event_rx) = broadcast::channel(16);
    let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

    // Start the track
    tts_track.start(event_tx, packet_tx).await?;

    let base64_text = tokio::fs::read("fixtures/base64noise.txt").await?;
    // remove the last '\n'
    let text = String::from_utf8(base64_text[..base64_text.len() - 1].to_vec())?;

    // Send a TTS command
    command_tx.send(SynthesisCommand {
        text,
        base64: true,
        ..Default::default()
    })?;

    let mut bytes_received = 0;
    let timeout = tokio::time::sleep(Duration::from_millis(3000));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => {
                break;
            }
            event = event_rx.recv() => {
                match event {
                    Ok(SessionEvent::TrackEnd { .. }) => {
                        break;
                    }
                    Err(_) => {
                        break;
                    }
                    _ => {}
                }
            }
            packet = packet_rx.recv() => {
                match packet {
                    Some(packet) => {
                        if let Samples::PCM { samples } = &packet.samples {
                            bytes_received += samples.len();
                        }
                    }
                    None => {
                        break
                    }
                }
            }
        }
    }
    assert!(bytes_received >= 16000, "Not enough bytes");
    Ok(())
}
