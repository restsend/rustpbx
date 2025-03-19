use crate::media::{
    processor::Processor,
    stream::{EventSender, MediaStreamBuilder},
    track::{Track, TrackId, TrackPacket, TrackPacketSender},
};
use anyhow::Result;
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
use tokio::{select, sync::Semaphore, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::info;
struct TestEmptyTrack {
    id: TrackId,
    started: Arc<Semaphore>,
    stopped: Arc<Semaphore>,
}

impl TestEmptyTrack {}

#[async_trait]
impl Track for TestEmptyTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }
    fn with_processors(&self, _processors: Vec<Box<dyn Processor>>) {}

    async fn start(
        &self,
        _token: CancellationToken,
        _event_sender: EventSender,
        _packet_sender: TrackPacketSender,
    ) -> Result<()> {
        info!("TestEmptyTrack started");
        self.started.add_permits(1);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("TestEmptyTrack stopped");
        self.stopped.add_permits(1);
        Ok(())
    }

    async fn send_packet(&self, _packet: &TrackPacket) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn test_empty_stream() {
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .try_init()
        .ok();
    let stream = Arc::new(MediaStreamBuilder::new().build());
    let stream_ref = stream.clone();
    tokio::spawn(async move {
        stream_ref.serve().await.unwrap();
    });
    let started = Arc::new(Semaphore::new(0));
    let stopped = Arc::new(Semaphore::new(0));
    let track = Box::new(TestEmptyTrack {
        id: "test".to_string(),
        started: started.clone(),
        stopped: stopped.clone(),
    });

    select! {
        _ = async {
            stream.update_track(track);
            let _ = started.acquire().await.expect("has_started");
            stream.remove_track("test".to_string());
            let _ = stopped.acquire().await.expect("has_stopped");
        } => {}
        _ = sleep(Duration::from_millis(500)) => {
            assert!(false, "test timed out");
        }
    }
}
