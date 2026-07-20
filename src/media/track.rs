use anyhow::Result;
use async_trait::async_trait;
use audio_codec::CodecType;
use rustrtc::PeerConnection;
use rustrtc::media::SampleStreamSource;

use crate::media::negotiate;

#[async_trait]
pub trait Track: Send + Sync {
    fn id(&self) -> &str;
    async fn handshake(&self, remote_offer: String) -> Result<String>;
    async fn local_description(&self) -> Result<String>;
    async fn set_remote_description(&self, remote: &str) -> Result<()>;
    async fn stop(&self);
    async fn get_peer_connection(&self) -> Option<PeerConnection>;
    fn set_codec_preference(&mut self, _codecs: Vec<CodecType>) {
    }
    fn preferred_codec_info(&self) -> Option<negotiate::CodecInfo> {
        None
    }

    /// Allow downcasting to concrete types for dynamic audio source switching
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Set muted state for this track
    /// Returns true if the operation was successful
    async fn set_muted(&self, _muted: bool) -> bool {
        false
    }

    /// Get current muted state
    fn is_muted(&self) -> bool {
        false
    }

    /// Get the media sample sender for this track, if available.
    /// This allows external code to inject audio into the track's PeerConnection.
    fn get_sender(&self) -> Option<SampleStreamSource> {
        None
    }
}
