use super::codecs::CodecType;
use crate::event::EventSender;
use crate::media::processor::Processor;
use crate::{AudioFrame, TrackId};
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
    pub server_side_track_id: TrackId,
}

impl Default for TrackConfig {
    fn default() -> Self {
        Self {
            codec: CodecType::PCMU,
            ptime: Duration::from_millis(20),
            samplerate: 16000,
            channels: 1,
            server_side_track_id: "server-side-track".to_string(),
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

    pub fn with_server_side_track_id(mut self, server_side_track_id: TrackId) -> Self {
        self.server_side_track_id = server_side_track_id;
        self
    }
}

pub mod file;
pub mod rtp;
pub mod track_codec;
pub mod tts;
pub mod webrtc;
pub mod websocket;
#[async_trait]
pub trait Track: Send + Sync {
    fn id(&self) -> &TrackId;
    fn config(&self) -> &TrackConfig;
    fn insert_processor(&mut self, processor: Box<dyn Processor>);
    fn append_processor(&mut self, processor: Box<dyn Processor>);
    async fn handshake(&mut self, offer: String, timeout: Option<Duration>) -> Result<String>;
    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_packet(&self, packet: &AudioFrame) -> Result<()>;

    /// Set remote SDP description (default implementation does nothing)
    /// RTP tracks should override this to set remote address
    fn set_remote_sdp(&mut self, _sdp: &str) -> Result<()> {
        Ok(())
    }
}
