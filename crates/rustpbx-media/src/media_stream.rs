use anyhow::{Result, anyhow};
use dashmap::DashMap;
use rustrtc::SdpType;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::rtc_track::RtcTrack;

pub struct MediaStreamBuilder {
    id: Option<String>,
    cancel_token: Option<CancellationToken>,
}

impl Default for MediaStreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaStreamBuilder {
    pub fn new() -> Self {
        Self {
            id: None,
            cancel_token: None,
        }
    }

    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn build(self) -> MediaStream {
        MediaStream {
            id: self.id.unwrap_or_else(|| "media-stream".to_string()),
            cancel_token: self.cancel_token.unwrap_or_default(),
            tracks: DashMap::new(),
        }
    }
}

pub struct MediaStream {
    pub id: String,
    pub cancel_token: CancellationToken,
    tracks: DashMap<String, Arc<RtcTrack>>,
}

impl MediaStream {
    pub async fn update_track(&self, track: RtcTrack, play_id: Option<String>) {
        let id = track.id().to_string();
        let wrapped = Arc::new(track);
        self.tracks.insert(id.clone(), wrapped.clone());
        if let Some(play_id) = play_id {
            debug!(track_id = %id, play_id = %play_id, "track updated (playback id)");
        }
    }

    pub async fn get_tracks(&self) -> Vec<Arc<RtcTrack>> {
        self.tracks.iter().map(|e| e.value().clone()).collect()
    }

    pub async fn update_remote_description(
        &self,
        track_id: &str,
        remote: &str,
        sdp_type: SdpType,
    ) -> Result<()> {
        let track = self.tracks.get(track_id).map(|e| e.value().clone());
        let Some(track) = track else {
            return Err(anyhow!("track not found: {track_id}"));
        };
        track.set_remote_description(remote, sdp_type).await
    }

    /// Mute a track by ID
    /// Returns true if the track was found and muted
    pub async fn mute_track(&self, track_id: &str) -> bool {
        match self.tracks.get(track_id).map(|e| e.value().clone()) {
            Some(track) => track.set_muted(true).await,
            None => false,
        }
    }

    /// Unmute a track by ID
    /// Returns true if the track was found and unmuted
    pub async fn unmute_track(&self, track_id: &str) -> bool {
        match self.tracks.get(track_id).map(|e| e.value().clone()) {
            Some(track) => track.set_muted(false).await,
            None => false,
        }
    }
}
