use super::codecs::CodecType;
use crate::event::EventSender;
use crate::media::processor::{Processor, ProcessorChain};
use crate::media::{AudioFrame, TrackId};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Duration;

pub type TrackPacketSender = mpsc::UnboundedSender<AudioFrame>;
pub type TrackPacketReceiver = mpsc::UnboundedReceiver<AudioFrame>;

// New shared track configuration struct
#[derive(Debug, Clone)]
pub struct TrackConfig {
    pub codec: CodecType,
    // Packet time in milliseconds (typically 10, 20, or 30ms)
    pub ptime: Duration,
    // Sample rate for PCM audio (e.g., 8000, 16000, 48000)
    pub samplerate: u32,
    // Number of audio channels (1 for mono, 2 for stereo)
    pub channels: u16,
}

impl Default for TrackConfig {
    fn default() -> Self {
        Self {
            codec: CodecType::PCMU,
            ptime: Duration::from_millis(20),
            samplerate: 16000,
            channels: 1,
        }
    }
}

impl TrackConfig {
    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.ptime = ptime;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.samplerate = sample_rate;
        self
    }

    pub fn with_channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }
}

pub mod file;
pub mod media_pass;
pub mod rtp;
pub mod track_codec;
pub mod tts;
pub mod webrtc;
pub mod websocket;
#[async_trait]
pub trait Track: Send + Sync {
    fn ssrc(&self) -> u32;
    fn id(&self) -> &TrackId;
    fn config(&self) -> &TrackConfig;
    fn processor_chain(&mut self) -> &mut ProcessorChain;
    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain().insert_processor(processor);
    }
    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain().append_processor(processor);
    }
    async fn handshake(&mut self, offer: String, timeout: Option<Duration>) -> Result<String>;
    async fn update_remote_description(&mut self, answer: &String) -> Result<()>;
    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn stop_graceful(&self) -> Result<()> {
        self.stop().await
    }
    async fn send_packet(&self, packet: &AudioFrame) -> Result<()>;
}
