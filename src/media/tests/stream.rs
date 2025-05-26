use crate::media::processor::ProcessorChain;
use crate::media::recorder::RecorderOption;
use crate::media::track::TrackConfig;
use crate::{
    event::EventSender,
    media::{
        processor::Processor,
        stream::MediaStreamBuilder,
        track::{Track, TrackPacketSender},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::time::Duration;

pub struct TestTrack {
    id: TrackId,
    config: TrackConfig,
    sender: Option<TrackPacketSender>,
    processor_chain: ProcessorChain,
    received_packets: Arc<Mutex<Vec<AudioFrame>>>,
}

impl TestTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            config: TrackConfig::default(),
            sender: None,
            processor_chain: ProcessorChain::new(16000),
            received_packets: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Track for TestTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }
    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn start(
        &self,
        _event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Store the packet sender for later use
        if let Some(sender) = unsafe { (self as *const _ as *mut TestTrack).as_mut() } {
            sender.sender = Some(packet_sender);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        {
            let mut received = self.received_packets.lock().await;
            received.push(packet.clone());
        }

        // Clone and process the packet
        let packet_clone = packet.clone();

        // Apply processors to the packet
        if let Err(e) = self.processor_chain.process_frame(&packet_clone) {
            tracing::error!("Error processing packet: {}", e);
        }

        if let Some(sender) = &self.sender {
            match sender.send(packet_clone) {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("Failed to send packet: {}", e);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stream_add_track() {
        let event_sender = crate::event::create_event_sender();
        let stream = MediaStreamBuilder::new(event_sender).build();
        let track = Box::new(TestTrack::new("test1".to_string()));
        stream.update_track(track).await;
    }

    #[tokio::test]
    async fn test_stream_remove_track() {
        let event_sender = crate::event::create_event_sender();
        let stream = MediaStreamBuilder::new(event_sender.clone())
            .with_id("ms:test".to_string())
            .build();
        let track_id = "test1".to_string();
        stream
            .update_track(Box::new(TestTrack::new(track_id.clone())))
            .await;
        stream.remove_track(&track_id).await;
    }
}

#[tokio::test]
async fn test_media_stream_basic() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Add a test track
    let track = Box::new(TestTrack::new("test1".to_string()));

    stream.update_track(track).await;

    // Start the stream
    let handle = tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Wait a bit
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_media_stream_events() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    let _events = stream.subscribe();

    // Add a test track
    let track = Box::new(TestTrack::new("test1".to_string()));

    stream.update_track(track).await;

    // Start the stream
    let handle = tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Wait a bit
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// New test for track packet forwarding
#[tokio::test]
async fn test_stream_forward_packets() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    let stream = MediaStreamBuilder::new(event_sender).build();

    // Create two test tracks
    let track1 = TestTrack::new("test1".to_string());
    let track2 = TestTrack::new("test2".to_string());

    // Get the track ID for the test packet
    let track2_id = track2.id().clone();

    // Add tracks to the stream
    stream.update_track(Box::new(track1)).await;
    stream.update_track(Box::new(track2)).await;
    let packet_sender = stream.packet_sender.clone();

    // Start the stream in a background task
    let handle = tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Allow time for setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send PCM data through the sender
    let samples = vec![16000, 8000, 12000, 4000];
    let packet = AudioFrame {
        track_id: track2_id.clone(),
        timestamp: 1000,
        samples: Samples::PCM { samples: samples },
        sample_rate: 16000,
    };

    // Try to send the packet - ignore errors
    let _ = packet_sender.send(packet);

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// Test for the Recorder functionality
#[tokio::test]
async fn test_stream_recorder() -> Result<()> {
    let event_sender = crate::event::create_event_sender();
    // Create a stream with recorder enabled

    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_recording.wav");
    let stream = Arc::new(
        MediaStreamBuilder::new(event_sender)
            .with_recorder_config(RecorderOption {
                recorder_file: file_path.to_string_lossy().to_string(),
                ..Default::default()
            })
            .build(),
    );

    // Create two test tracks
    let track1 = Box::new(TestTrack::new("test1".to_string()));
    let track2 = Box::new(TestTrack::new("test2".to_string()));

    // Get the track ID for the test packet
    let track2_id = track2.id().clone();

    // Add tracks to the stream
    stream.update_track(track1).await;
    stream.update_track(track2).await;

    // Clone the stream for the background task
    let stream_clone = stream.clone();

    // Start the stream in a background task
    let handle = tokio::spawn(async move {
        stream_clone.serve().await.unwrap();
    });

    // Allow time for setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Get access to the internal packet sender
    let packet_sender = stream.packet_sender.clone();

    // Send multiple PCM packets with different samples
    let samples1 = vec![3000, 6000, 9000, 12000];
    let samples2 = vec![15000, 18000, 21000, 24000];

    // Create the packets
    let packet1 = AudioFrame {
        track_id: track2_id.clone(),
        timestamp: 1000,
        samples: Samples::PCM { samples: samples1 },
        sample_rate: 16000,
    };

    let packet2 = AudioFrame {
        track_id: track2_id,
        timestamp: 1020,
        samples: Samples::PCM { samples: samples2 },
        sample_rate: 16000,
    };

    // Send the packets directly to the packet sender
    packet_sender.send(packet1).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    packet_sender.send(packet2).unwrap();

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// Test for forwarding between different payload types
#[tokio::test]
async fn test_stream_forward_payload_conversion() -> Result<()> {
    // Create a stream
    let event_sender = crate::event::create_event_sender();
    let stream = Arc::new(MediaStreamBuilder::new(event_sender).build());

    // Create two test tracks with different packet types
    let track1 = TestTrack::new("track1".to_string()); // This will receive PCM
    let track2 = TestTrack::new("track2".to_string()); // This will send RTP

    // Add tracks to the stream
    stream.update_track(Box::new(track1)).await;
    stream.update_track(Box::new(track2)).await;

    // Start the stream in a background task
    let stream_clone = stream.clone();
    let handle = tokio::spawn(async move {
        stream_clone.serve().await.unwrap();
    });

    // Allow time for setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Get access to the internal packet sender
    let packet_sender = stream.packet_sender.clone();

    // Create an RTP packet from track2
    let rtp_packet = AudioFrame {
        track_id: "track2".to_string(),
        timestamp: 1000,
        samples: Samples::RTP {
            payload_type: 0,
            payload: vec![1, 2, 3, 4],
            sequence_number: 1,
        },
        sample_rate: 16000,
    };

    // Send the RTP packet - ignore errors
    let _ = packet_sender.send(rtp_packet);

    // Create a PCM packet from track1
    let pcm_packet = AudioFrame {
        track_id: "track1".to_string(),
        timestamp: 2000,
        samples: Samples::PCM {
            samples: vec![3000, 6000, 9000, 12000],
        },
        sample_rate: 16000,
    };

    // Send the PCM packet - ignore errors
    let _ = packet_sender.send(pcm_packet);

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}
