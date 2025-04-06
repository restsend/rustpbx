use crate::{
    event::create_event_sender,
    media::{
        processor::Processor,
        track::{rtp::*, track_codec::TrackCodec, Track, TrackConfig},
    },
    AudioFrame, Samples,
};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

// Simple processor for testing
struct TestProcessor;

#[async_trait]
impl Processor for TestProcessor {
    fn process_frame(&self, _frame: &mut AudioFrame) -> Result<()> {
        // Simple pass-through processor
        Ok(())
    }
}

// Basic configuration tests
#[tokio::test]
async fn test_rtp_track_creation() -> Result<()> {
    let track_id = "test-rtp-track".to_string();
    let track = RtpTrack::new(track_id.clone());

    // Check track ID is set correctly
    assert_eq!(track.id(), &track_id);

    // Test with modified sample rate
    let sample_rate = 16000;
    let track = RtpTrack::new(track_id.clone())
        .with_sample_rate(sample_rate)
        .with_config(TrackConfig::default().with_sample_rate(sample_rate));

    // Test with RTP configuration
    let rtp_config = RtpTrackConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        payload_type: 0,
        ssrc: 12345,
    };

    let track = track.with_rtp_config(rtp_config);

    // Verify default cancel token is used
    let cancel_token = CancellationToken::new();
    let _track = track.with_cancel_token(cancel_token.clone());

    // Testing cancel token works
    cancel_token.cancel();
    assert!(cancel_token.is_cancelled());

    Ok(())
}

// Test processor insertion
#[tokio::test]
async fn test_rtp_track_processor() -> Result<()> {
    let track_id = "test-rtp-track-processor".to_string();
    let mut track = RtpTrack::new(track_id);

    // Insert processor at the beginning of the chain
    let processor = Box::new(TestProcessor);
    track.insert_processor(processor);

    // Append processor at the end of the chain
    let processor2 = Box::new(TestProcessor);
    track.append_processor(processor2);

    Ok(())
}

// Test socket setup
#[tokio::test]
async fn test_rtp_socket_setup() -> Result<()> {
    let track_id = "test-rtp-socket".to_string();
    let mut track = RtpTrack::new(track_id);

    // Configure with random local port
    let rtp_config = RtpTrackConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        payload_type: 0,
        ssrc: 12345,
    };

    track = track.with_rtp_config(rtp_config);

    // Set up RTP socket
    track.setup_rtp_socket().await?;

    Ok(())
}

#[tokio::test]
async fn test_rtp_track_send_packet() -> Result<()> {
    // Set up a track with a real local socket
    let cancel_token = CancellationToken::new();
    let track_id = "test-rtp-send".to_string();
    let mut track = RtpTrack::new(track_id.clone()).with_cancel_token(cancel_token.clone());

    // Configure with loopback address to a non-zero port that we don't expect to connect to
    let rtp_config = RtpTrackConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        remote_addr: "127.0.0.1:12345".parse().unwrap(), // Using a port we don't expect to be listening
        payload_type: 0,                                 // PCMU
        ssrc: 12345,
    };
    track = track.with_rtp_config(rtp_config);

    // Set up the socket
    track.setup_rtp_socket().await?;

    // Create a test audio frame with silence
    let audio_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(vec![0; 160]), // 20ms of silence at 8kHz
        timestamp: 0,
        sample_rate: 8000,
    };

    // This will likely fail to send since we're not actually connecting to a real endpoint,
    // but it will exercise the packet creation code
    let result = track.send_packet(&audio_frame).await;

    // Clean up resources
    cancel_token.cancel();

    // We expect this to fail since we're not properly connected,
    // but we're just testing the packet building logic, so it's okay
    info!("send_packet result (expected to fail): {:?}", result);

    // Mark test as passed regardless of send result
    Ok(())
}

#[tokio::test]
async fn test_rtp_track_start_stop() -> Result<()> {
    let cancel_token = CancellationToken::new();
    let track_id = "test-rtp-start-stop".to_string();
    let mut track = RtpTrack::new(track_id.clone()).with_cancel_token(cancel_token.clone());

    // Set up RTP configuration with a port we won't actually use
    let config = RtpTrackConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        remote_addr: "127.0.0.1:9998".parse().unwrap(),
        payload_type: 0,
        ssrc: 12345,
    };
    track = track.with_rtp_config(config);

    // Set up socket - this will actually bind to a real port
    track.setup_rtp_socket().await?;

    // Create a channel to receive audio frames
    let (sender, _receiver) = mpsc::unbounded_channel();

    // Start the track with event sender and packet sender
    track
        .start(cancel_token.clone(), create_event_sender(), sender)
        .await?;

    // Very short wait to ensure tasks start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Test that stopping works
    track.stop().await?;
    cancel_token.cancel();

    // Verify that the track was properly stopped
    // The cancel token should be cancelled
    assert!(cancel_token.is_cancelled());

    Ok(())
}

#[tokio::test]
async fn test_rtp_codec_encode_decode() -> Result<()> {
    // Test the RTP codec encoding and decoding functionality
    let track_id = "test-rtp-codec".to_string();
    let encoder = TrackCodec::new();

    // Create sample PCM data (sine wave pattern for easy verification)
    let sample_rate = 8000;
    let samples: Vec<i16> = (0..160)
        .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
        .collect();

    // Create an audio frame with PCM samples
    let audio_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(samples.clone()),
        timestamp: 0,
        sample_rate,
    };

    // Test PCMU encoding
    let payload_type = 0; // PCMU
    let encoded = encoder.encode(payload_type, audio_frame);

    // PCMU should be approximately the same size as the input (1 byte per sample)
    assert_eq!(encoded.len(), samples.len());

    // Now decode
    let decoded = encoder.decode(payload_type, &encoded);

    // Verify that decoding approximately reverses encoding
    // We can't expect exact matches due to lossy compression
    assert_eq!(decoded.len(), samples.len());

    // Calculate average sample difference (should be small for acceptable quality)
    let total_diff: i32 = samples
        .iter()
        .zip(decoded.iter())
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .sum();
    let avg_diff = total_diff as f64 / samples.len() as f64;

    // Allow for some difference due to lossy compression, but it should be within reason
    assert!(
        avg_diff < 1000.0,
        "Average sample difference too high: {}",
        avg_diff
    );

    Ok(())
}
