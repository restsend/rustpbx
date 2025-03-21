use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::media::{
    processor::{AudioFrame, Processor},
    stream::{EventSender, MediaStreamBuilder, MediaStreamEvent},
    track::{Track, TrackId, TrackPacket, TrackPacketSender, TrackPayload},
};

pub struct TestTrack {
    id: TrackId,
    sender: Option<TrackPacketSender>,
    receivers: Vec<mpsc::UnboundedReceiver<TrackPacket>>,
    processors: Vec<Box<dyn Processor>>,
    received_packets: Arc<Mutex<Vec<TrackPacket>>>,
}

impl TestTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            sender: None,
            receivers: Vec::new(),
            processors: Vec::new(),
            received_packets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    // Helper method to create a PCM packet
    pub fn create_pcm_packet(&self, samples: Vec<i16>, timestamp: u64) -> TrackPacket {
        TrackPacket {
            track_id: self.id.clone(),
            timestamp,
            payload: TrackPayload::PCM(samples),
        }
    }

    // Helper method to create an RTP packet
    pub fn create_rtp_packet(
        &self,
        payload_type: u8,
        payload: Vec<u8>,
        timestamp: u64,
    ) -> TrackPacket {
        TrackPacket {
            track_id: self.id.clone(),
            timestamp,
            payload: TrackPayload::RTP(payload_type, payload),
        }
    }

    // Method to send PCM data through the track
    pub async fn send_pcm(&self, samples: Vec<i16>, timestamp: u64) -> Result<()> {
        if let Some(sender) = &self.sender {
            let packet = self.create_pcm_packet(samples, timestamp);
            sender.send(packet)?;
        }
        Ok(())
    }

    // Method to send RTP data through the track
    pub async fn send_rtp(&self, payload_type: u8, payload: Vec<u8>, timestamp: u64) -> Result<()> {
        if let Some(sender) = &self.sender {
            let packet = self.create_rtp_packet(payload_type, payload, timestamp);
            sender.send(packet)?;
        }
        Ok(())
    }

    // Method to get received packets
    pub async fn get_received_packets(&self) -> Vec<TrackPacket> {
        self.received_packets.lock().await.clone()
    }
}

#[async_trait]
impl Track for TestTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors = processors;
    }

    fn processors(&self) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .map(|p| p.as_ref() as &dyn Processor)
            .collect()
    }

    async fn start(
        &self,
        _token: CancellationToken,
        _event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Store the sender for later use
        let this = if let Some(this) = unsafe { (self as *const Self as *mut Self).as_mut() } {
            this
        } else {
            return Ok(());
        };
        this.sender = Some(packet_sender);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn send_packet(&self, packet: &TrackPacket) -> Result<()> {
        // Store the received packet
        let mut packets = self.received_packets.lock().await;
        packets.push(packet.clone());

        // Process the packet with processors before sending
        if !self.processors.is_empty() {
            if let TrackPayload::PCM(samples) = &packet.payload {
                let mut frame = AudioFrame {
                    track_id: packet.track_id.clone(),
                    samples: samples.clone(),
                    timestamp: packet.timestamp as u32,
                    sample_rate: 16000,
                };

                for processor in &self.processors {
                    let _ = processor.process_frame(&mut frame);
                }
            }
        }

        // Forward to all receivers
        for receiver in &self.receivers {
            if let Some(sender) = unsafe {
                (receiver as *const _ as *mut mpsc::UnboundedSender<TrackPacket>).as_mut()
            } {
                let _ = sender.send(packet.clone());
            }
        }
        Ok(())
    }

    async fn recv_packet(&self) -> Option<TrackPacket> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_stream_add_track() {
        let stream = MediaStreamBuilder::new().build();
        let track = Box::new(TestTrack::new("test1".to_string()));
        stream.update_track(track).await;
    }

    #[tokio::test]
    async fn test_stream_remove_track() {
        let stream = MediaStreamBuilder::new().build();
        let track = Box::new(TestTrack::new("test1".to_string()));
        stream.update_track(track).await;
        stream.remove_track(&"test1".to_string()).await;
    }
}

#[tokio::test]
async fn test_media_stream_basic() -> Result<()> {
    let stream = MediaStreamBuilder::new().build();

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
    let stream = MediaStreamBuilder::new().event_buf_size(32).build();

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
    let stream = Arc::new(MediaStreamBuilder::new().build());

    // Create two test tracks
    let track1 = TestTrack::new("test1".to_string());
    let track2 = TestTrack::new("test2".to_string());

    // Get the track ID for the test packet
    let track2_id = track2.id().clone();

    // Add tracks to the stream
    stream.update_track(Box::new(track1)).await;
    stream.update_track(Box::new(track2)).await;

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

    // Send PCM data through the sender
    let samples = vec![16000, 8000, 12000, 4000];
    let packet = TrackPacket {
        track_id: track2_id.clone(),
        timestamp: 1000,
        payload: TrackPayload::PCM(samples),
    };
    packet_sender.send(packet).unwrap();

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}

// Test for the Recorder functionality
#[tokio::test]
async fn test_stream_recorder() -> Result<()> {
    // Create a stream with recorder enabled
    let stream = Arc::new(
        MediaStreamBuilder::new()
            .recorder("test_recording".to_string())
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
    let packet1 = TrackPacket {
        track_id: track2_id.clone(),
        timestamp: 1000,
        payload: TrackPayload::PCM(samples1),
    };

    let packet2 = TrackPacket {
        track_id: track2_id,
        timestamp: 1020,
        payload: TrackPayload::PCM(samples2),
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
    let stream = Arc::new(MediaStreamBuilder::new().build());

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
    let rtp_packet = TrackPacket {
        track_id: "track2".to_string(),
        timestamp: 1000,
        payload: TrackPayload::RTP(0, vec![1, 2, 3, 4]), // Simple payload for testing
    };

    // Send the RTP packet
    packet_sender.send(rtp_packet).unwrap();

    // Create a PCM packet from track1
    let pcm_packet = TrackPacket {
        track_id: "track1".to_string(),
        timestamp: 2000,
        payload: TrackPayload::PCM(vec![3000, 6000, 9000, 12000]),
    };

    // Send the PCM packet
    packet_sender.send(pcm_packet).unwrap();

    // Allow time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Stop the stream
    handle.abort();

    Ok(())
}
