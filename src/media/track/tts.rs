use crate::media::{
    processor::{AudioFrame, Processor},
    stream::EventSender,
    track::{Track, TrackId, TrackPacketSender},
};
use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub struct TtsTrack {
    id: TrackId,
    processors: Vec<Box<dyn Processor>>,
}

impl TtsTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            processors: Vec::new(),
        }
    }
}

#[async_trait]
impl Track for TtsTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
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
        _packet_sender: TrackPacketSender,
    ) -> Result<()> {
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}
