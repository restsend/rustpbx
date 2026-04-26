use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use rustrtc::{
    Attribute, IceServer, IceTransportPolicy, MediaKind, PeerConnection, RtcConfiguration,
    RtpCodecParameters, SdpType, SessionDescription, TransceiverDirection, TransportMode,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
pub use transcoder::Transcoder;

use crate::media::recorder::RecorderOption;

pub type TrackMap = HashMap<String, Arc<AsyncMutex<Box<dyn Track>>>>;

pub mod audio_source;
pub mod bridge;
#[cfg(test)]
mod file_track_tests;
pub mod forwarding_track;
pub mod mixer;
#[cfg(test)]
mod mixer_e2e_tests;
pub mod mixer_input;
pub mod mixer_output;
pub mod mixer_registry;
pub mod negotiate;
pub mod sdp_bridge;
pub mod audio_egress_track;
pub mod transcoder;
pub mod transcoding_pipeline;
#[cfg(test)]
mod unified_pc_tests;
pub mod wav_writer;

pub trait StreamWriter: Send + Sync {
    fn write_header(&mut self) -> Result<()>;
    fn write_packet(&mut self, data: &[u8], samples: usize) -> Result<()>;
    fn finalize(&mut self) -> Result<()>;
}

pub fn get_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}

fn codec_info_rtpmap(info: &negotiate::CodecInfo) -> String {
    let codec_name = match info.codec {
        CodecType::PCMU => "PCMU",
        CodecType::PCMA => "PCMA",
        CodecType::G722 => "G722",
        CodecType::G729 => "G729",
        #[cfg(feature = "opus")]
        CodecType::Opus => "opus",
        CodecType::TelephoneEvent => "telephone-event",
    };

    match info.channels {
        0 | 1 => format!("{}/{}", codec_name, info.clock_rate),
        channels => format!("{}/{}/{}", codec_name, info.clock_rate, channels),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioFrameTiming {
    pcm_sample_rate: u32,
    pcm_samples_per_frame: usize,
    rtp_ticks_per_frame: u32,
}

fn audio_frame_timing(codec: CodecType, rtp_clock_rate: u32) -> AudioFrameTiming {
    let frame_ms = 20u32;
    let pcm_sample_rate = codec.samplerate();

    AudioFrameTiming {
        pcm_sample_rate,
        pcm_samples_per_frame: (pcm_sample_rate * frame_ms / 1000) as usize,
        rtp_ticks_per_frame: rtp_clock_rate * frame_ms / 1000,
    }
}

#[async_trait]
pub trait Track: Send + Sync {
    fn id(&self) -> &str;
    async fn handshake(&self, remote_offer: String) -> Result<String>;
    async fn local_description(&self) -> Result<String>;
    async fn set_remote_description(&self, remote: &str) -> Result<()>;
    async fn stop(&self);
    async fn get_peer_connection(&self) -> Option<rustrtc::PeerConnection>;
    async fn set_recorder_option(&mut self, _option: RecorderOption) {}
    fn set_codec_preference(&mut self, _codecs: Vec<CodecType>) {
        // Optional: override to set codec preference
    }
    fn preferred_codec_info(&self) -> Option<negotiate::CodecInfo> {
        None
    }

    /// Allow downcasting to concrete types for dynamic audio source switching
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        unimplemented!("as_any_mut not implemented for this Track type")
    }

    /// Set muted state for this track
    /// Returns true if the operation was successful
    async fn set_muted(&self, _muted: bool) -> bool {
        // Default implementation does nothing
        // Concrete types should override this
        false
    }

    /// Get current muted state
    fn is_muted(&self) -> bool {
        // Default implementation returns false
        false
    }
}

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
            tracks: Mutex::new(HashMap::new()),
            recorder_option: self.recorder_option,
        }
    }
}

pub struct MediaStream {
    pub id: String,
    pub cancel_token: CancellationToken,
    tracks: Mutex<TrackMap>,
    pub recorder_option: Option<RecorderOption>,
}

impl MediaStream {
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
        {
            let mut tracks = self.tracks.lock().unwrap();
            tracks.insert(id.clone(), wrapped.clone());
        }
        if let Some(play_id) = play_id {
            debug!(track_id = %id, play_id = %play_id, "track updated (playback id)");
        }
    }

    pub async fn get_tracks(&self) -> Vec<Arc<AsyncMutex<Box<dyn Track>>>> {
        let tracks = self.tracks.lock().unwrap();
        tracks.values().cloned().collect()
    }

    pub async fn update_remote_description(&self, track_id: &str, remote: &str) -> Result<()> {
        let handle = {
            let tracks = self.tracks.lock().unwrap();
            tracks.get(track_id).cloned()
        };
        let Some(track) = handle else {
            return Err(anyhow!("track not found: {track_id}"));
        };
        let guard = track.lock().await;
        guard.set_remote_description(remote).await
    }

    pub async fn remove_track(&self, track_id: &str, _stop_audio_immediately: bool) {
        let mut tracks = self.tracks.lock().unwrap();
        tracks.remove(track_id);
    }

    /// Mute a track by ID
    /// Returns true if the track was found and muted
    pub async fn mute_track(&self, track_id: &str) -> bool {
        // Get track handle while holding the lock, then release the lock before await
        let track_handle = {
            let tracks = self.tracks.lock().unwrap();
            tracks.get(track_id).cloned()
        };

        if let Some(track) = track_handle {
            let guard = track.lock().await;
            guard.set_muted(true).await
        } else {
            false
        }
    }

    /// Unmute a track by ID
    /// Returns true if the track was found and unmuted
    pub async fn unmute_track(&self, track_id: &str) -> bool {
        // Get track handle while holding the lock, then release the lock before await
        let track_handle = {
            let tracks = self.tracks.lock().unwrap();
            tracks.get(track_id).cloned()
        };

        if let Some(track) = track_handle {
            let guard = track.lock().await;
            guard.set_muted(false).await
        } else {
            false
        }
    }
}

pub struct RtcTrack {
    track_id: String,
    pc: PeerConnection,
    pub recorder_option: Option<RecorderOption>,
    rtp_map: Vec<negotiate::CodecInfo>,
    muted: std::sync::atomic::AtomicBool,
}

impl RtcTrack {
    pub fn new(
        track_id: String,
        config: RtcConfiguration,
        rtp_map: Vec<negotiate::CodecInfo>,
    ) -> Self {
        let pc = PeerConnection::new(config);

        // Add a dummy track to ensure a sender is created and SSRC is signaled in SDP
        let (_, track, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 100);
        let mut params = RtpCodecParameters::default();
        if let Some(info) = rtp_map.first() {
            params.payload_type = info.payload_type;
            params.clock_rate = info.clock_rate;
            params.channels = info.channels as u8;
        }
        let _ = pc.add_track(track, params);

        Self {
            track_id,
            pc,
            recorder_option: None,
            rtp_map,
            muted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn new_with_video(
        track_id: String,
        config: RtcConfiguration,
        rtp_map: Vec<negotiate::CodecInfo>,
        video_capabilities: Vec<rustrtc::config::VideoCapability>,
    ) -> Self {
        let pc = PeerConnection::new(config);

        // Add audio track
        let (_, audio_track, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 100);
        let mut audio_params = RtpCodecParameters::default();
        if let Some(info) = rtp_map.first() {
            audio_params.payload_type = info.payload_type;
            audio_params.clock_rate = info.clock_rate;
            audio_params.channels = info.channels as u8;
        }
        let _ = pc.add_track(audio_track, audio_params);

        // Add video track for each video capability
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

        Self {
            track_id,
            pc,
            recorder_option: None,
            rtp_map,
            muted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_recorder_option(mut self, option: RecorderOption) -> Self {
        self.recorder_option = Some(option);
        self
    }

    async fn set_local(&self, pc: &PeerConnection, mut desc: SessionDescription) -> Result<String> {
        if !self.rtp_map.is_empty()
            && let Some(section) = desc
                .media_sections
                .iter_mut()
                .find(|m| m.kind == MediaKind::Audio)
            {
                section.formats.clear();
                section
                    .attributes
                    .retain(|a| a.key != "rtpmap" && a.key != "fmtp");

                // Build RTP map from codec preference list
                let mut seen_pts = HashSet::new();
                for info in self.rtp_map.iter() {
                    let pt = info.payload_type;
                    if !seen_pts.insert(pt) {
                        continue;
                    }
                    section.formats.push(pt.to_string());

                    section.attributes.push(Attribute {
                        key: "rtpmap".to_string(),
                        value: Some(format!("{} {}", pt, codec_info_rtpmap(info))),
                    });
                    if let Some(fmtp) = info.codec.fmtp() {
                        section.attributes.push(Attribute {
                            key: "fmtp".to_string(),
                            value: Some(format!("{} {}", pt, fmtp)),
                        });
                    }
                }
            }
        pc.set_local_description(desc)?;
        let desc = pc
            .local_description()
            .ok_or_else(|| anyhow!("missing local description"))?;
        Ok(desc.to_sdp_string())
    }

    async fn set_remote(&self, pc: &PeerConnection, sdp: &str, ty: SdpType) -> Result<()> {
        let desc = SessionDescription::parse(ty, sdp)
            .map_err(|e| anyhow!("failed to parse sdp: {:?}", e))?;
        match pc.set_remote_description(desc).await {
            Ok(_) => (),
            Err(e) => {
                warn!(
                    track_id = self.track_id,
                    error = %e,
                    "failed to set remote description"
                );
            }
        }
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
                    && let Some(desc) = self.pc.local_description() {
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
        // Note: Actual RTP mute is handled at the media stream level
        // by dropping or zeroing audio packets
        true
    }

    fn is_muted(&self) -> bool {
        self.muted.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for RtcTrack {
    fn drop(&mut self) {
        debug!(track_id = %self.track_id, "RtcTrack dropping, closing PeerConnection");
        self.pc.close();
    }
}

pub mod recorder;

#[cfg(test)]
mod recorder_tests;

pub struct RtpTrackBuilder {
    track_id: String,
    cancel_token: Option<CancellationToken>,
    external_ip: Option<String>,
    bind_ip: Option<String>,
    rtp_start_port: Option<u16>,
    rtp_end_port: Option<u16>,
    mode: TransportMode,
    rtp_map: Vec<negotiate::CodecInfo>,
    video_capabilities: Vec<rustrtc::config::VideoCapability>,
    enable_latching: bool,
    ice_servers: Vec<IceServer>,
}

impl RtpTrackBuilder {
    pub fn new(track_id: String) -> Self {
        Self {
            track_id,
            cancel_token: None,
            external_ip: None,
            bind_ip: None,
            rtp_start_port: None,
            rtp_end_port: None,
            mode: TransportMode::Rtp,
            enable_latching: false,
            ice_servers: Vec::new(),
            rtp_map: vec![
                #[cfg(feature = "opus")]
                CodecType::Opus,
                CodecType::G729,
                CodecType::G722,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::TelephoneEvent,
            ]
            .into_iter()
            .map(|c| negotiate::CodecInfo {
                payload_type: c.payload_type(),
                clock_rate: c.clock_rate(),
                channels: c.channels(),
                codec: c,
            })
            .collect(),
            video_capabilities: Vec::new(),
        }
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }
    pub fn with_rtp_range(mut self, start: u16, end: u16) -> Self {
        self.rtp_start_port = Some(start);
        self.rtp_end_port = Some(end);
        self
    }

    pub fn with_mode(mut self, mode: TransportMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_external_ip(mut self, addr: String) -> Self {
        self.external_ip = Some(addr);
        self
    }

    pub fn with_bind_ip(mut self, addr: String) -> Self {
        self.bind_ip = Some(addr);
        self
    }

    pub fn with_codec_preference(mut self, codecs: Vec<CodecType>) -> Self {
        self.rtp_map = codecs
            .into_iter()
            .map(|c| negotiate::CodecInfo {
                payload_type: c.payload_type(),
                clock_rate: c.clock_rate(),
                channels: c.channels(),
                codec: c,
            })
            .collect();
        self
    }

    pub fn with_codec_info(mut self, codecs: Vec<negotiate::CodecInfo>) -> Self {
        self.rtp_map = codecs;
        self
    }

    pub fn with_enable_latching(mut self, enable: bool) -> Self {
        self.enable_latching = enable;
        self
    }

    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    /// Set video capabilities for the PeerConnection.
    /// Controls which video codecs appear in SDP offers/answers.
    pub fn with_video_capabilities(mut self, caps: Vec<rustrtc::config::VideoCapability>) -> Self {
        self.video_capabilities = caps;
        self
    }

    pub fn build(self) -> RtcTrack {
        // Choose SDP compatibility mode based on transport mode:
        // - WebRTC mode: use Standard (includes rtcp-mux, a=mid)
        // - RTP/SRTP mode: use LegacySip (omits rtcp-mux for Linphone compatibility)
        let sdp_compatibility = match self.mode {
            TransportMode::WebRtc => rustrtc::config::SdpCompatibilityMode::Standard,
            TransportMode::Rtp | TransportMode::Srtp => {
                rustrtc::config::SdpCompatibilityMode::LegacySip
            }
        };

        let has_turn_server = self.ice_servers.iter().any(|server| {
            server.urls.iter().any(|url| {
                let u = url.trim_start().to_ascii_lowercase();
                u.starts_with("turn:") || u.starts_with("turns:")
            })
        });
        let audio_capabilities = match self.mode {
            TransportMode::WebRtc => negotiate::MediaNegotiator::default_webrtc_codecs(),
            TransportMode::Rtp | TransportMode::Srtp => {
                negotiate::MediaNegotiator::default_rtp_codecs()
            }
        }
        .into_iter()
        .filter_map(|codec| {
            negotiate::CodecInfo {
                payload_type: codec.payload_type(),
                clock_rate: codec.clock_rate(),
                channels: codec.channels(),
                codec,
            }
            .to_audio_capability()
        })
        .collect();

        let bind_ip = if matches!(self.mode, TransportMode::Rtp | TransportMode::Srtp) {
            self.bind_ip
        } else {
            None
        };

        let config = RtcConfiguration {
            ice_servers: self.ice_servers,
            ice_transport_policy: if self.mode == TransportMode::WebRtc && has_turn_server {
                IceTransportPolicy::Relay
            } else {
                IceTransportPolicy::All
            },
            transport_mode: self.mode,
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
            external_ip: self.external_ip,
            bind_ip,
            enable_latching: self.enable_latching,
            media_capabilities: Some(rustrtc::config::MediaCapabilities {
                audio: audio_capabilities,
                video: self.video_capabilities.clone(),
                application: None,
            }),
            ssrc_start: rand::random::<u32>(),
            sdp_compatibility,
            ..Default::default()
        };

        if self.video_capabilities.is_empty() {
            RtcTrack::new(self.track_id, config, self.rtp_map)
        } else {
            RtcTrack::new_with_video(self.track_id, config, self.rtp_map, self.video_capabilities)
        }
    }
}

/// Audio file playback track with loop support
///
/// Used for playing audio files (e.g., ringback tones, hold music, announcements).
pub struct FileTrack {
    track_id: String,
    file_path: Option<String>,
    loop_playback: bool,
    cancel_token: CancellationToken,
    pc: PeerConnection,
    completion_notify: Arc<tokio::sync::Notify>,
    codec_preference: Vec<CodecType>,
    codec_info: Option<negotiate::CodecInfo>,
    mode: TransportMode,
    rtp_start_port: Option<u16>,
    rtp_end_port: Option<u16>,
    external_ip: Option<String>,
    bind_ip: Option<String>,
    audio_source_manager: Option<Arc<audio_source::AudioSourceManager>>,
    muted: std::sync::atomic::AtomicBool,
}

impl Clone for FileTrack {
    fn clone(&self) -> Self {
        Self {
            track_id: self.track_id.clone(),
            file_path: self.file_path.clone(),
            loop_playback: self.loop_playback,
            cancel_token: self.cancel_token.clone(),
            pc: self.pc.clone(),
            completion_notify: self.completion_notify.clone(),
            codec_preference: self.codec_preference.clone(),
            codec_info: self.codec_info.clone(),
            mode: self.mode.clone(),
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
            external_ip: self.external_ip.clone(),
            bind_ip: self.bind_ip.clone(),
            audio_source_manager: self.audio_source_manager.clone(),
            muted: std::sync::atomic::AtomicBool::new(
                self.muted.load(std::sync::atomic::Ordering::Relaxed),
            ),
        }
    }
}

impl FileTrack {
    pub fn new(track_id: String) -> Self {
        let config = RtcConfiguration {
            transport_mode: TransportMode::Rtp,
            ..Default::default()
        };

        let pc = PeerConnection::new(config);
        pc.add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly);

        Self {
            track_id,
            file_path: None,
            loop_playback: false,
            cancel_token: CancellationToken::new(),
            pc,
            completion_notify: Arc::new(tokio::sync::Notify::new()),
            codec_preference: vec![CodecType::PCMU, CodecType::PCMA],
            codec_info: None,
            mode: TransportMode::Rtp,
            rtp_start_port: None,
            rtp_end_port: None,
            external_ip: None,
            bind_ip: None,
            audio_source_manager: None,
            muted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn with_path(mut self, path: String) -> Self {
        self.file_path = Some(path);
        self
    }

    pub fn with_loop(mut self, loop_playback: bool) -> Self {
        self.loop_playback = loop_playback;
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = token;
        self
    }

    pub fn with_codec_preference(mut self, codecs: Vec<CodecType>) -> Self {
        self.codec_preference = codecs;
        self
    }

    pub fn with_codec_info(mut self, info: negotiate::CodecInfo) -> Self {
        self.codec_info = Some(info);
        self
    }

    pub fn with_mode(mut self, mode: TransportMode) -> Self {
        self.mode = mode;
        self.recreate_pc();
        self
    }

    pub fn with_rtp_range(mut self, start: u16, end: u16) -> Self {
        self.rtp_start_port = Some(start);
        self.rtp_end_port = Some(end);
        self.recreate_pc();
        self
    }

    pub fn with_external_ip(mut self, ip: String) -> Self {
        self.external_ip = Some(ip);
        self.recreate_pc();
        self
    }

    pub fn with_bind_ip(mut self, ip: String) -> Self {
        self.bind_ip = Some(ip);
        self.recreate_pc();
        self
    }

    fn recreate_pc(&mut self) {
        let bind_ip = if matches!(self.mode, TransportMode::Rtp | TransportMode::Srtp) {
            self.bind_ip.clone()
        } else {
            None
        };

        let config = RtcConfiguration {
            transport_mode: self.mode.clone(),
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
            external_ip: self.external_ip.clone(),
            bind_ip,
            ssrc_start: rand::random::<u32>(),
            ..Default::default()
        };

        self.pc = PeerConnection::new(config);
        self.pc
            .add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly);
    }

    pub fn with_ssrc(self, _ssrc: u32) -> Self {
        self
    }

    pub async fn wait_for_completion(&self) {
        self.completion_notify.notified().await;
    }

    fn init_audio_source(&mut self) -> Result<()> {
        if self.audio_source_manager.is_some() {
            return Ok(());
        }

        let target_sample_rate = self
            .codec_info
            .as_ref()
            .map(|info| info.codec.samplerate())
            .or_else(|| {
                self.codec_preference
                    .first()
                    .map(|codec| codec.samplerate())
            })
            .unwrap_or(8000);

        let manager = Arc::new(audio_source::AudioSourceManager::new(target_sample_rate));

        if let Some(ref path) = self.file_path {
            manager.switch_to_file(path.clone(), self.loop_playback)?;
        } else {
            manager.switch_to_silence();
        }

        self.audio_source_manager = Some(manager);
        Ok(())
    }

    pub async fn start_playback(&self) -> Result<()> {
        self.start_playback_on(None).await
    }
    pub async fn start_playback_on(&self, target_pc: Option<PeerConnection>) -> Result<()> {
        use audio_codec::create_encoder;
        use rustrtc::media::{AudioFrame, MediaSample};

        let file_path = self
            .file_path
            .as_ref()
            .ok_or_else(|| anyhow!("No file path set"))?;

        // Allow remote URLs through — FileAudioSource handles downloading.
        let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
        if !is_remote && !std::path::Path::new(file_path).exists() {
            return Err(anyhow!("Audio file not found: {}", file_path));
        }

        // Determine the codec/payload type we will encode to.
        let selected = self.codec_info.clone().unwrap_or_else(|| {
            let codec = self
                .codec_preference
                .first()
                .copied()
                .unwrap_or(CodecType::PCMU);
            negotiate::CodecInfo {
                payload_type: codec.payload_type(),
                codec,
                clock_rate: codec.clock_rate(),
                channels: codec.channels(),
            }
        });
        let codec = selected.codec;
        let payload_type = selected.payload_type;
        let frame_timing = audio_frame_timing(codec, selected.clock_rate);
        let samples_per_frame = frame_timing.pcm_samples_per_frame;

        // Use the caller's negotiated PC when provided, otherwise fall back to
        // the FileTrack's own (un-negotiated) PC.
        let has_external_pc = target_pc.is_some();
        let pc = target_pc.unwrap_or_else(|| self.pc.clone());

        debug!(
            file = %file_path,
            loop_playback = self.loop_playback,
            ?codec,
            samples_per_frame,
            pcm_sample_rate = frame_timing.pcm_sample_rate,
            rtp_ticks_per_frame = frame_timing.rtp_ticks_per_frame,
            has_external_pc,
            "FileTrack start_playback_on"
        );

        // Initialise the audio source manager (idempotent if already done).
        let audio_source_manager = {
            // We need a &mut self to call init_audio_source, but start_playback
            // takes &self.  Clone the manager if already initialised, or build
            // one inline here.
            if let Some(ref mgr) = self.audio_source_manager {
                mgr.clone()
            } else {
                let mgr = Arc::new(audio_source::AudioSourceManager::new(
                    frame_timing.pcm_sample_rate,
                ));
                mgr.switch_to_file(file_path.clone(), self.loop_playback)?;
                mgr
            }
        };

        // Create a sample_track pair.  `source_target` is the write end we
        // push frames into; `track_target` is the MediaStreamTrack we register
        // with the PeerConnection sender.
        let (source_target, track_target, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 100);

        // Wire track_target into the PC's existing Send transceiver.
        let transceivers = pc.get_transceivers();
        let existing = transceivers.iter().find(|t| t.kind() == MediaKind::Audio);

        let ssrc = rand::random::<u32>();
        let params = RtpCodecParameters {
            payload_type,
            clock_rate: selected.clock_rate,
            channels: selected.channels as u8,
        };

        if let Some(transceiver) = existing {
            let track_arc: Arc<dyn rustrtc::media::MediaStreamTrack> = track_target;
            let new_sender = rustrtc::RtpSender::builder(track_arc, ssrc)
                .params(params)
                .build();
            transceiver.set_sender(Some(new_sender));
        } else {
            let _ = pc.add_track(track_target, params);
        }

        // Clone everything we need to move into the background task.
        let completion_notify = self.completion_notify.clone();
        let cancel_token = self.cancel_token.clone();
        let loop_playback = self.loop_playback;

        crate::utils::spawn(async move {
            let mut encoder = create_encoder(codec);
            let mut rtp_timestamp: u32 = rand::random();
            let mut sequence_number: u16 = rand::random();
            let interval_ms = 20u64;
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!("FileTrack playback cancelled");
                        completion_notify.notify_waiters();
                        break;
                    }
                    _ = interval.tick() => {
                        // Read one frame of PCM samples from the source.
                        let mut pcm_buf = vec![0i16; samples_per_frame];
                        let read = audio_source_manager.read_samples(&mut pcm_buf);

                        if read == 0 {
                            // Source exhausted — completion_notify was already
                            // triggered by AudioSourceManager::read_samples.
                            // For non-looping sources we also notify here in case
                            // the manager chose not to.
                            if !loop_playback {
                                debug!("FileTrack playback completed (file exhausted)");
                                completion_notify.notify_waiters();
                                break;
                            }
                            // Looping sources restart automatically; keep going.
                            continue;
                        }

                        // Encode PCM → target codec.
                        let encoded = encoder.encode(&pcm_buf[..read]);

                        let frame = AudioFrame {
                            rtp_timestamp,
                            clock_rate: selected.clock_rate,
                            data: encoded.into(),
                            sequence_number: Some(sequence_number),
                            payload_type: Some(payload_type),
                            marker: false,
                            header_extension: None,
                            raw_packet: None,
                            source_addr: None,
                        };

                        rtp_timestamp =
                            rtp_timestamp.wrapping_add(frame_timing.rtp_ticks_per_frame);
                        sequence_number = sequence_number.wrapping_add(1);

                        if let Err(e) = source_target.send(MediaSample::Audio(frame)).await {
                            debug!("FileTrack source_target.send failed (receiver gone): {}", e);
                            completion_notify.notify_waiters();
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }

    pub fn switch_audio_source(&mut self, file_path: String, loop_playback: bool) -> Result<()> {
        if self.audio_source_manager.is_none() {
            self.init_audio_source()?;
        }

        if let Some(ref manager) = self.audio_source_manager {
            manager.switch_to_file(file_path, loop_playback)?;
        }

        Ok(())
    }

    pub fn switch_to_silence(&mut self) {
        if let Some(ref manager) = self.audio_source_manager {
            manager.switch_to_silence();
        }
    }
}

#[async_trait]
impl Track for FileTrack {
    fn id(&self) -> &str {
        &self.track_id
    }

    async fn handshake(&self, remote_offer: String) -> Result<String> {
        self.pc.wait_for_gathering_complete().await;

        let offer = SessionDescription::parse(SdpType::Offer, &remote_offer)?;

        self.pc.set_remote_description(offer).await?;
        let answer = self.pc.create_answer().await?;
        self.pc.set_local_description(answer.clone())?;

        Ok(answer.to_sdp_string())
    }

    async fn local_description(&self) -> Result<String> {
        self.pc.wait_for_gathering_complete().await;

        let mut offer = self.pc.create_offer().await?;

        if !self.codec_preference.is_empty()
            && let Some(section) = offer
                .media_sections
                .iter_mut()
                .find(|m| m.kind == MediaKind::Audio)
            {
                section.formats.clear();
                section
                    .attributes
                    .retain(|a| a.key != "rtpmap" && a.key != "fmtp");

                let mut seen_pts = HashSet::new();
                for codec in &self.codec_preference {
                    let pt = codec.payload_type();
                    if !seen_pts.insert(pt) {
                        continue;
                    }
                    let pt_str = pt.to_string();
                    section.formats.push(pt_str.clone());

                    section.attributes.push(Attribute {
                        key: "rtpmap".to_string(),
                        value: Some(format!("{} {}", pt_str, codec.rtpmap())),
                    });
                    if let Some(fmtp) = codec.fmtp() {
                        section.attributes.push(Attribute {
                            key: "fmtp".to_string(),
                            value: Some(format!("{} {}", pt_str, fmtp)),
                        });
                    }
                }
            }

        self.pc.set_local_description(offer.clone())?;
        Ok(offer.to_sdp_string())
    }

    async fn set_remote_description(&self, remote: &str) -> Result<()> {
        self.pc.wait_for_gathering_complete().await;
        let desc = SessionDescription::parse(SdpType::Answer, remote)?;
        self.pc.set_remote_description(desc).await?;
        Ok(())
    }

    async fn stop(&self) {
        self.cancel_token.cancel();
    }

    async fn get_peer_connection(&self) -> Option<PeerConnection> {
        Some(self.pc.clone())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn set_muted(&self, muted: bool) -> bool {
        self.muted
            .store(muted, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn is_muted(&self) -> bool {
        self.muted.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for FileTrack {
    fn drop(&mut self) {
        debug!(track_id = %self.track_id, "FileTrack dropping, closing PeerConnection");
        self.cancel_token.cancel();
        self.pc.close();
    }
}

pub mod conference_mixer;

#[cfg(test)]
mod media_track_tests;
