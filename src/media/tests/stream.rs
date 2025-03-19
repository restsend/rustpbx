use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::media::{
    processor::Processor,
    stream::{EventSender, MediaStreamBuilder},
    track::{Track, TrackId, TrackPacket, TrackPacketSender},
};

pub struct TestTrack {
    id: TrackId,
}

impl TestTrack {
    pub fn new(id: TrackId) -> Self {
        Self { id }
    }
}

#[async_trait]
impl Track for TestTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {}

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

    async fn send_packet(&self, _packet: &TrackPacket) -> Result<()> {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_stream_add_track() {
        let stream = MediaStreamBuilder::new().build();
        let track = Box::new(TestTrack::new("test1".to_string()));
        stream.update_track(track);
    }

    #[tokio::test]
    async fn test_stream_remove_track() {
        let stream = MediaStreamBuilder::new().build();
        let track = Box::new(TestTrack::new("test1".to_string()));
        stream.update_track(track);
        stream.remove_track(&"test1".to_string()).await;
    }
}

#[tokio::test]
async fn test_media_stream_basic() -> Result<()> {
    let stream = MediaStreamBuilder::new().build();

    // Add a test track
    let track = Box::new(TestTrack::new("test1".to_string()));

    stream.update_track(track);

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

    let mut events = stream.subscribe();

    // Add a test track
    let track = Box::new(TestTrack::new("test1".to_string()));

    stream.update_track(track);

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
