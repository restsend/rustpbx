use anyhow::{Result, anyhow};
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::media::Track;
use crate::media::recorder::RecorderOption;

pub type TrackMap = DashMap<String, Arc<AsyncMutex<Box<dyn Track>>>>;

pub struct MediaStreamBuilder {
    id: Option<String>,
    cancel_token: Option<CancellationToken>,
    recorder_option: Option<RecorderOption>,
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
            recorder_option: None,
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

    pub fn with_recorder_config(mut self, option: RecorderOption) -> Self {
        self.recorder_option = Some(option);
        self
    }

    pub fn build(self) -> MediaStream {
        MediaStream {
            id: self.id.unwrap_or_else(|| "media-stream".to_string()),
            cancel_token: self.cancel_token.unwrap_or_default(),
            tracks: DashMap::new(),
            recorder_option: self.recorder_option,
        }
    }
}

pub struct MediaStream {
    pub id: String,
    pub cancel_token: CancellationToken,
    tracks: DashMap<String, Arc<AsyncMutex<Box<dyn Track>>>>,
    pub recorder_option: Option<RecorderOption>,
}

impl MediaStream {
    #[allow(dead_code)]
    pub async fn serve(&self) -> Result<()> {
        self.cancel_token.cancelled().await;
        Ok(())
    }

    pub async fn update_track(&self, mut track: Box<dyn Track>, play_id: Option<String>) {
        if let Some(ref option) = self.recorder_option {
            track.set_recorder_option(option.clone()).await;
        }
        let id = track.id().to_string();
        let wrapped = Arc::new(AsyncMutex::new(track));
        self.tracks.insert(id.clone(), wrapped.clone());
        if let Some(play_id) = play_id {
            debug!(track_id = %id, play_id = %play_id, "track updated (playback id)");
        }
    }

    pub async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>> {
        self.tracks.iter().map(|e| e.value().clone()).collect()
    }

    pub async fn update_remote_description(&self, track_id: &str, remote: &str) -> Result<()> {
        let track = self.tracks.get(track_id).map(|e| e.value().clone());
        let Some(track) = track else {
            return Err(anyhow!("track not found: {track_id}"));
        };
        let guard = track.lock().await;
        guard.set_remote_description(remote).await
    }

    pub async fn remove_track(&self, track_id: &str, _stop_audio_immediately: bool) {
        self.tracks.remove(track_id);
    }

    /// Mute a track by ID
    /// Returns true if the track was found and muted
    pub async fn mute_track(&self, track_id: &str) -> bool {
        let track = self.tracks.get(track_id).map(|e| e.value().clone());
        if let Some(track) = track {
            let guard = track.lock().await;
            guard.set_muted(true).await
        } else {
            false
        }
    }

    /// Unmute a track by ID
    /// Returns true if the track was found and unmuted
    pub async fn unmute_track(&self, track_id: &str) -> bool {
        let track = self.tracks.get(track_id).map(|e| e.value().clone());
        if let Some(track) = track {
            let guard = track.lock().await;
            guard.set_muted(false).await
        } else {
            false
        }
    }
}
