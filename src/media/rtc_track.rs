use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rustrtc::{
    PeerConnection, RtcConfiguration, RtpCodecParameters,
    SdpType, SessionDescription,
    media::SampleStreamSource,
};
use tracing::debug;

use crate::media::Track;
use crate::media::negotiate;

pub struct RtcTrack {
    track_id: String,
    pc: PeerConnection,
    rtp_map: Vec<negotiate::CodecInfo>,
    muted: std::sync::atomic::AtomicBool,
    sender: Option<SampleStreamSource>,
}

impl RtcTrack {
    pub fn new(
        track_id: String,
        config: RtcConfiguration,
        rtp_map: Vec<negotiate::CodecInfo>,
    ) -> Self {
        let _guard = crate::utils::media_enter();
        let pc = PeerConnection::new(config);

        let (tx, track, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 100);
        let mut params = RtpCodecParameters::default();
        if let Some(info) = rtp_map.first() {
            params.payload_type = info.payload_type;
            params.clock_rate = info.clock_rate;
            params.channels = info.channels as u8;
        }
        let _ = pc.add_track(track, params);
        drop(_guard);

        Self {
            track_id,
            pc,
            rtp_map,
            muted: std::sync::atomic::AtomicBool::new(false),
            sender: Some(tx),
        }
    }

    pub fn new_with_video(
        track_id: String,
        config: RtcConfiguration,
        rtp_map: Vec<negotiate::CodecInfo>,
        video_capabilities: Vec<rustrtc::config::VideoCapability>,
    ) -> Self {
        let _guard = crate::utils::media_enter();
        let pc = PeerConnection::new(config);

        let (tx, audio_track, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 100);
        let mut audio_params = RtpCodecParameters::default();
        if let Some(info) = rtp_map.first() {
            audio_params.payload_type = info.payload_type;
            audio_params.clock_rate = info.clock_rate;
            audio_params.channels = info.channels as u8;
        }
        let _ = pc.add_track(audio_track, audio_params);

        for video_cap in video_capabilities.iter() {
            let (_, video_track, _) =
                rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Video, 100);
            let video_params = RtpCodecParameters {
                payload_type: video_cap.payload_type,
                clock_rate: video_cap.clock_rate,
                channels: 0,
            };
            let _ = pc.add_track(video_track, video_params);
        }
        drop(_guard);

        Self {
            track_id,
            pc,
            rtp_map,
            muted: std::sync::atomic::AtomicBool::new(false),
            sender: Some(tx),
        }
    }

    async fn set_local(&self, pc: &PeerConnection, desc: SessionDescription) -> Result<String> {
        pc.set_local_description(desc)?;
        let desc = pc
            .local_description()
            .ok_or_else(|| anyhow!("missing local description"))?;
        Ok(desc.to_sdp_string())
    }

    async fn set_remote(&self, pc: &PeerConnection, sdp: &str, ty: SdpType) -> Result<()> {
        let desc = SessionDescription::parse(ty, sdp)
            .map_err(|e| anyhow!("failed to parse sdp: {:?}", e))?;
        pc.set_remote_description(desc)
            .await
            .map_err(|e| anyhow!("failed to set remote description: {}", e))?;
        Ok(())
    }
}

#[async_trait]
impl Track for RtcTrack {
    fn id(&self) -> &str {
        &self.track_id
    }

    async fn handshake(&self, remote_offer: String) -> Result<String> {
        self.pc.wait_for_gathering_complete().await;
        self.set_remote(&self.pc, &remote_offer, SdpType::Offer)
            .await?;
        let answer = self.pc.create_answer().await?;
        let sdp = self.set_local(&self.pc, answer).await?;
        Ok(sdp)
    }

    async fn local_description(&self) -> Result<String> {
        self.pc.wait_for_gathering_complete().await;
        match self.pc.create_offer().await {
            Ok(offer) => {
                let sdp = self.set_local(&self.pc, offer).await?;
                Ok(sdp)
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("HaveLocalOffer")
                    && let Some(desc) = self.pc.local_description()
                {
                    return Ok(desc.to_sdp_string());
                }
                Err(anyhow!(e))
            }
        }
    }

    async fn set_remote_description(&self, remote: &str) -> Result<()> {
        self.pc.wait_for_gathering_complete().await;
        self.set_remote(&self.pc, remote, SdpType::Answer).await
    }

    async fn stop(&self) {
        self.pc.close();
    }

    async fn get_peer_connection(&self) -> Option<PeerConnection> {
        Some(self.pc.clone())
    }

    fn preferred_codec_info(&self) -> Option<negotiate::CodecInfo> {
        self.rtp_map.first().cloned()
    }

    async fn set_muted(&self, muted: bool) -> bool {
        self.muted
            .store(muted, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn is_muted(&self) -> bool {
        self.muted.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn get_sender(&self) -> Option<SampleStreamSource> {
        self.sender.clone()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Drop for RtcTrack {
    fn drop(&mut self) {
        debug!(track_id = %self.track_id, "RtcTrack dropping, closing PeerConnection");
        self.pc.close();
    }
}
