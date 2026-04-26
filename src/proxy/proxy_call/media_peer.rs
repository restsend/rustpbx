use crate::media::audio_route::{self, AudioEgressSlot};
use crate::media::audio_egress_track::AudioEgressTrack;
use crate::media::audio_egress_track::AudioInputTap;
use crate::media::negotiate::NegotiatedLegProfile;
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
    async fn remove_track(&self, track_id: &str, stop: bool);
    async fn serve(&self) -> Result<()>;
    fn stop(&self);

    /// Mute a track by ID
    /// Returns true if the track was found and muted
    async fn mute_track(&self, track_id: &str) -> bool;

    /// Unmute a track by ID
    /// Returns true if the track was found and unmuted
    async fn unmute_track(&self, track_id: &str) -> bool;
}

pub struct LegMedia {
    cancel_token: CancellationToken,
    peer_connection: AsyncMutex<Option<rustrtc::PeerConnection>>,
    audio_egress: AudioEgressSlot,
}

impl LegMedia {
    pub fn new(cancel_token: CancellationToken) -> Self {
        Self {
            cancel_token,
            peer_connection: AsyncMutex::new(None),
            audio_egress: AsyncMutex::new(None),
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    pub async fn set_peer_connection(&self, pc: rustrtc::PeerConnection) {
        *self.peer_connection.lock().await = Some(pc);
    }

    pub async fn peer_connection(&self) -> Option<rustrtc::PeerConnection> {
        self.peer_connection.lock().await.clone()
    }

    pub async fn input_audio_track(&self) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
        let pc = self.peer_connection().await?;
        audio_route::input_audio_track(&pc)
    }

    pub async fn ensure_audio_egress(
        &self,
        egress_profile: NegotiatedLegProfile,
        session_id: &str,
        direction: &str,
    ) -> Result<Arc<AudioEgressTrack>> {
        let target_pc = self
            .peer_connection()
            .await
            .ok_or_else(|| anyhow::anyhow!("{}: no PeerConnection", direction))?;
        audio_route::ensure_audio_egress(
            &self.audio_egress,
            &target_pc,
            egress_profile,
            session_id,
            direction,
        )
        .await
    }

    pub async fn audio_egress(&self) -> Option<Arc<AudioEgressTrack>> {
        self.audio_egress.lock().await.clone()
    }

    pub async fn stage_peer_source(
        &self,
        source_pc: &rustrtc::PeerConnection,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
        session_id: &str,
        direction: &str,
    ) -> Result<()> {
        let target_pc = self
            .peer_connection()
            .await
            .ok_or_else(|| anyhow::anyhow!("{}: no PeerConnection", direction))?;
        audio_route::stage_peer_audio_route(
            source_pc,
            &self.audio_egress,
            &target_pc,
            ingress_profile,
            egress_profile,
            tap,
            session_id,
            direction,
        )
        .await
    }

    pub async fn clear_peer_connection(&self) {
        *self.peer_connection.lock().await = None;
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

pub struct VoiceEnginePeer {
    stream: Arc<MediaStream>,
    leg_media: Arc<LegMedia>,
}

impl VoiceEnginePeer {
    pub fn new(stream: Arc<MediaStream>) -> Self {
        let leg_media = Arc::new(LegMedia::new(stream.cancel_token.clone()));
        Self { stream, leg_media }
    }

    pub fn with_leg_media(stream: Arc<MediaStream>, leg_media: Arc<LegMedia>) -> Self {
        Self { stream, leg_media }
    }
}

#[async_trait]
impl MediaPeer for VoiceEnginePeer {
    fn cancel_token(&self) -> CancellationToken {
        self.leg_media.cancel_token()
    }

    async fn update_track(&self, track: Box<dyn Track>, play_id: Option<String>) {
        if let Some(pc) = track.get_peer_connection().await {
            self.leg_media.set_peer_connection(pc).await;
        }
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

    async fn remove_track(&self, track_id: &str, stop: bool) {
        self.leg_media.clear_peer_connection().await;
        self.stream.remove_track(track_id, stop).await;
    }

    async fn serve(&self) -> Result<()> {
        self.stream.serve().await
    }

    fn stop(&self) {
        self.leg_media.stop();
    }

    async fn mute_track(&self, track_id: &str) -> bool {
        self.stream.mute_track(track_id).await
    }

    async fn unmute_track(&self, track_id: &str) -> bool {
        self.stream.unmute_track(track_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn audio_egress_track_replaces_sender_stably() {
        let pc = rustrtc::PeerConnection::new(rustrtc::RtcConfiguration::default());
        let (_, track, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 1);
        let params = rustrtc::RtpCodecParameters {
            payload_type: 0,
            clock_rate: 8000,
            channels: 1,
        };
        let original_sender = pc.add_track(track, params).expect("sender should be added");
        let output_slot = AudioEgressSlot::new(None);

        let output = audio_route::ensure_audio_egress(
            &output_slot,
            &pc,
            NegotiatedLegProfile::default(),
            "test-session",
            "callee→caller",
        )
        .await
        .expect("audio egress track should install");

        let sender = pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Audio)
            .and_then(|t| t.sender())
            .expect("audio sender should exist");
        assert_eq!(sender.track_id(), "audio-egress");
        assert_eq!(sender.ssrc(), original_sender.ssrc());

        let output_again = audio_route::ensure_audio_egress(
            &output_slot,
            &pc,
            NegotiatedLegProfile::default(),
            "test-session",
            "callee→caller",
        )
        .await
        .expect("existing audio egress track should be reused");

        assert!(Arc::ptr_eq(&output, &output_again));
    }
}
