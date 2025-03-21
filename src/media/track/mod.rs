use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
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
