use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::media::processor::Processor;

use super::stream::EventSender;

#[derive(Debug, Clone)]
pub enum TrackPayload {
    PCM(Vec<i16>),
    RTP(u8, Vec<u8>),
}
#[derive(Debug, Clone)]
pub struct TrackPacket {
    pub track_id: TrackId,
    pub timestamp: u64,
    pub payload: TrackPayload,
}

pub type TrackPacketSender = mpsc::UnboundedSender<TrackPacket>;
pub type TrackPacketReceiver = mpsc::UnboundedReceiver<TrackPacket>;

// New shared track configuration struct
#[derive(Debug, Clone)]
pub struct TrackConfig {
    // Packet time in milliseconds (typically 10, 20, or 30ms)
    pub ptime: Duration,
    // Sample rate for PCM audio (e.g., 8000, 16000, 48000)
    pub sample_rate: u32,
    // Number of audio channels (1 for mono, 2 for stereo)
    pub channels: u16,
    // Maximum size of PCM chunks to process at once
    pub max_pcm_chunk_size: usize,
}

impl Default for TrackConfig {
    fn default() -> Self {
        Self {
            ptime: Duration::from_millis(20),
            sample_rate: 16000,
            channels: 1,
            max_pcm_chunk_size: 320, // 20ms at 16kHz
        }
    }
}

impl TrackConfig {
    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.ptime = ptime;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        // Update max_pcm_chunk_size based on sample rate and ptime
        self.max_pcm_chunk_size =
            ((sample_rate as f64 * self.ptime.as_secs_f64()) as usize).max(160);
        self
    }

    pub fn with_channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }
}

pub mod file;
pub mod rtp;
pub mod tts;
pub mod webrtc;

pub type TrackId = String;

#[async_trait]
pub trait Track: Send + Sync {
    fn id(&self) -> &TrackId;
    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>);
    fn processors(&self) -> Vec<&dyn Processor> {
        Vec::new()
    }
    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_packet(&self, packet: &TrackPacket) -> Result<()>;
    async fn recv_packet(&self) -> Option<TrackPacket>;
}
