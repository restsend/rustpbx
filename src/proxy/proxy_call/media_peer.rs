use crate::media::MediaStream;
use anyhow::Result;
use async_trait::async_trait;
use rustrtc::SdpType;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::media::RtcTrack;

#[async_trait]
pub trait MediaPeer: Send + Sync {
    fn cancel_token(&self) -> CancellationToken;
    async fn update_track(&self, track: RtcTrack, play_id: Option<String>);
    async fn get_tracks(&self) -> Vec<Arc<RtcTrack>>;
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

    async fn update_track(&self, track: RtcTrack, play_id: Option<String>) {
        self.update_track(track, play_id).await;
    }

    async fn get_tracks(&self) -> Vec<Arc<RtcTrack>> {
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
