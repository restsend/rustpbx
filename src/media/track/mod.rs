use super::{processor::Processor, stream::EventSender};
use ::webrtc::rtp::packet::Packet;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
mod file;
mod rtp;
mod tts;
mod webrtc;

pub type TrackId = String;
pub struct TrackPacket {
    pub track_id: TrackId,
    pub packet: Packet,
}
pub type TrackPacketSender = mpsc::UnboundedSender<TrackPacket>;
pub type TrackPacketReceiver = mpsc::UnboundedReceiver<TrackPacket>;

#[async_trait]
pub trait Track: Send + Sync {
    fn id(&self) -> &TrackId;
    fn with_processors(&self, processors: Vec<Box<dyn Processor>>);
    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_packet(&self, packet: &TrackPacket) -> Result<()>;
}
