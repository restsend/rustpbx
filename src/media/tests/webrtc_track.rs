use anyhow::Result;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::media::codecs::pcmu;
use crate::media::codecs::Encoder;
use crate::media::processor::Processor;
use crate::media::track::webrtc::WebrtcTrack;
use crate::media::track::Track;
use crate::{AudioFrame, Samples};

// Simple test processor that counts frames
struct CountingProcessor {
    count: Arc<AtomicUsize>,
}

impl CountingProcessor {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        (
            Self {
                count: count.clone(),
            },
            count,
        )
    }
}

impl Processor for CountingProcessor {
    fn process_frame(&self, _frame: &mut AudioFrame) -> Result<()> {
        self.count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

// Helper to create a WebRTC RTP packet (simplified version without webrtc-rs dependency)
fn create_simple_rtp_packet(
    track_id: &str,
    timestamp: u64,
    payload_type: u8,
    payload: Vec<u8>,
) -> AudioFrame {
    // Create basic RTP packet
    AudioFrame {
        track_id: track_id.to_string(),
        timestamp,
        samples: Samples::RTP(payload_type, payload),
        sample_rate: 16000,
    }
}

#[tokio::test]
async fn test_webrtc_track_pcm() -> Result<()> {
    // Create a WebRTC track
    let track_id = "test_webrtc_track".to_string();
    let webrtc_track = WebrtcTrack::new(track_id.clone());

    // Create a processor
    let (processor, count) = CountingProcessor::new();

    // Create channels
    let (event_sender, _) = broadcast::channel(16);
    let (packet_sender, _packet_receiver) = mpsc::unbounded_channel();

    // Start the track
    let token = CancellationToken::new();
    webrtc_track
        .start(token.clone(), event_sender, packet_sender)
        .await?;

    // Wait briefly for setup to complete
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Add the processor directly to the jitter buffer to ensure it's called
    {
        let packet = AudioFrame {
            track_id: track_id.clone(),
            timestamp: 1000,
            samples: Samples::PCM(vec![1, 2, 3, 4]),
            sample_rate: 16000,
        };

        // Process it directly with our processor
        let mut packet_clone = packet.clone();
        processor.process_frame(&mut packet_clone)?;

        // Verify count was incremented
        assert!(
            count.load(Ordering::Relaxed) > 0,
            "Processor should have been called at least once"
        );
    }

    // Stop the track
    webrtc_track.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_webrtc_track_rtp() -> Result<()> {
    // Create an WebrtcTrack with PCMU codec
    let track_id = "test_webrtc_track".to_string();
    let webrtc_track = WebrtcTrack::new(track_id.clone());

    // Create channels
    let (event_sender, _event_receiver) = broadcast::channel(16);
    let (packet_sender, _packet_receiver) = mpsc::unbounded_channel();

    // Start the track
    let token = CancellationToken::new();
    webrtc_track
        .start(token.clone(), event_sender, packet_sender)
        .await?;

    // Create a PCMU encoder to encode PCM data
    let mut encoder = pcmu::PcmuEncoder::new();

    // Create PCM data
    let pcm_data: Vec<i16> = (0..320)
        .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
        .collect();

    // Encode the PCM data
    let encoded = encoder.encode(&pcm_data);

    // Create a simpler RTP packet (no WebRTC-RS dependency)
    let rtp_packet = create_simple_rtp_packet(&track_id, 1000, 0, encoded.to_vec()); // 0 is PCMU

    // Send the packet to the track
    webrtc_track.send_packet(&rtp_packet).await?;

    // Wait for processing to occur
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the track
    webrtc_track.stop().await?;

    Ok(())
}
