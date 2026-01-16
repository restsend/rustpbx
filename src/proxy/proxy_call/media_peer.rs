use crate::media::{MediaStream, Track};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait MediaPeer: Send + Sync {
    fn cancel_token(&self) -> CancellationToken;
    async fn update_track(&self, track: Box<dyn Track>, play_id: Option<String>);
    async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>>;
    async fn update_remote_description(&self, track_id: &str, remote: &str) -> Result<()>;
    async fn suppress_forwarding(&self, track_id: &str);
    async fn resume_forwarding(&self, track_id: &str);
    fn is_suppressed(&self, track_id: &str) -> bool;
    async fn remove_track(&self, track_id: &str, stop: bool);
    async fn serve(&self) -> Result<()>;
    fn stop(&self);
}

pub struct VoiceEnginePeer {
    stream: Arc<MediaStream>,
}

impl VoiceEnginePeer {
    pub fn new(stream: Arc<MediaStream>) -> Self {
        Self { stream }
    }
}

#[async_trait]
impl MediaPeer for VoiceEnginePeer {
    fn cancel_token(&self) -> CancellationToken {
        self.stream.cancel_token.clone()
    }

    async fn update_track(&self, track: Box<dyn Track>, play_id: Option<String>) {
        self.stream.update_track(track, play_id).await;
    }

    async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>> {
        self.stream.get_tracks().await
    }

    async fn update_remote_description(&self, track_id: &str, remote: &str) -> Result<()> {
        self.stream
            .update_remote_description(track_id, remote)
            .await
    }

    async fn suppress_forwarding(&self, track_id: &str) {
        self.stream.suppress_forwarding(track_id).await;
    }

    async fn resume_forwarding(&self, track_id: &str) {
        self.stream.resume_forwarding(track_id).await;
    }

    fn is_suppressed(&self, track_id: &str) -> bool {
        self.stream.is_suppressed(track_id)
    }

    async fn remove_track(&self, track_id: &str, stop: bool) {
        self.stream.remove_track(track_id, stop).await;
    }

    async fn serve(&self) -> Result<()> {
        self.stream.serve().await
    }

    fn stop(&self) {
        self.stream.cancel_token.cancel();
    }
}
