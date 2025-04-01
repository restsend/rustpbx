use crate::event::EventSender;
use crate::media::processor::Processor;
use crate::{AudioFrame, TrackId};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

pub type TrackPacketSender = mpsc::UnboundedSender<AudioFrame>;
pub type TrackPacketReceiver = mpsc::UnboundedReceiver<AudioFrame>;

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
            max_pcm_chunk_size: 320, // 20ms at 16kHz, 16-bit mono
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

    pub fn with_max_pcm_chunk_size(mut self, max_pcm_chunk_size: usize) -> Self {
        self.max_pcm_chunk_size = max_pcm_chunk_size;
        self
    }
}

pub mod file;
pub mod rtp;
pub mod track_codec;
pub mod tts;
pub mod webrtc;

#[async_trait]
pub trait Track: Send + Sync {
    fn id(&self) -> &TrackId;
    fn insert_processor(&mut self, processor: Box<dyn Processor>);
    fn append_processor(&mut self, processor: Box<dyn Processor>);
    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_packet(&self, packet: &AudioFrame) -> Result<()>;
}
