use crate::media::{MediaStream, Track};
use anyhow::Result;
use async_trait::async_trait;
use rustrtc::SdpType;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait MediaPeer: Send + Sync {
    fn cancel_token(&self) -> CancellationToken;
    async fn update_track(&self, track: Box<dyn Track>, play_id: Option<String>);
    async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>>;
    async fn update_remote_description(
        &self,
        track_id: &str,
        remote: &str,
        sdp_type: SdpType,
    ) -> Result<()>;

    /// Mute a track by ID
    /// Returns true if the track was found and muted
    async fn mute_track(&self, track_id: &str) -> bool;

    /// Unmute a track by ID
    /// Returns true if the track was found and unmuted
    async fn unmute_track(&self, track_id: &str) -> bool;
}

#[async_trait]
impl MediaPeer for MediaStream {
    fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    async fn update_track(&self, track: Box<dyn Track>, play_id: Option<String>) {
        self.update_track(track, play_id).await;
    }

    async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>> {
        self.get_tracks().await
    }

    async fn update_remote_description(
        &self,
        track_id: &str,
        remote: &str,
        sdp_type: SdpType,
    ) -> Result<()> {
        self.update_remote_description(track_id, remote, sdp_type)
            .await
    }

    async fn mute_track(&self, track_id: &str) -> bool {
        self.mute_track(track_id).await
    }

    async fn unmute_track(&self, track_id: &str) -> bool {
        self.unmute_track(track_id).await
    }
}
