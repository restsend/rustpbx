use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use rustrtc::{
    Attribute, IceServer, MediaKind, PeerConnection, RtcConfiguration, RtpCodecParameters, SdpType,
    SessionDescription, TransceiverDirection, TransportMode,
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

pub mod audio_source;
#[cfg(test)]
mod file_track_tests;
pub mod negotiate;
pub mod transcoder;
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

    /// Allow downcasting to concrete types for dynamic audio source switching
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        unimplemented!("as_any_mut not implemented for this Track type")
    }
}

pub struct MediaStreamBuilder {
    id: Option<String>,
    cancel_token: Option<CancellationToken>,
    recorder_option: Option<RecorderOption>,
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
            cancel_token: self.cancel_token.unwrap_or_else(CancellationToken::new),
            tracks: Mutex::new(HashMap::new()),
            suppressed: Mutex::new(HashSet::new()),
            recorder_option: self.recorder_option,
        }
    }
}

pub struct MediaStream {
    pub id: String,
    pub cancel_token: CancellationToken,
    tracks: Mutex<HashMap<String, Arc<AsyncMutex<Box<dyn Track>>>>>,
    suppressed: Mutex<HashSet<String>>,
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

    pub async fn suppress_forwarding(&self, track_id: &str) {
        let mut suppressed = self.suppressed.lock().unwrap();
        suppressed.insert(track_id.to_string());
    }

    pub async fn resume_forwarding(&self, track_id: &str) {
        let mut suppressed = self.suppressed.lock().unwrap();
        suppressed.remove(track_id);
    }

    pub fn is_suppressed(&self, track_id: &str) -> bool {
        let suppressed = self.suppressed.lock().unwrap();
        suppressed.contains(track_id)
    }

    pub async fn remove_track(&self, track_id: &str, _stop_audio_immediately: bool) {
        let mut tracks = self.tracks.lock().unwrap();
        tracks.remove(track_id);
    }
}

pub struct RtcTrack {
    track_id: String,
    pc: PeerConnection,
    pub recorder_option: Option<RecorderOption>,
    rtp_map: Vec<negotiate::CodecInfo>,
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
        }
    }

    pub fn with_recorder_option(mut self, option: RecorderOption) -> Self {
        self.recorder_option = Some(option);
        self
    }

    async fn set_local(&self, pc: &PeerConnection, mut desc: SessionDescription) -> Result<String> {
        if !self.rtp_map.is_empty() {
            if let Some(section) = desc
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
                        value: Some(format!("{} {}", pt, info.codec.rtpmap())),
                    });
                    if let Some(fmtp) = info.codec.fmtp() {
                        section.attributes.push(Attribute {
                            key: "fmtp".to_string(),
                            value: Some(format!("{} {}", pt, fmtp)),
                        });
                    }
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
                if err_str.contains("HaveLocalOffer") {
                    if let Some(desc) = self.pc.local_description() {
                        return Ok(desc.to_sdp_string());
                    }
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
}

pub mod recorder;

#[cfg(test)]
mod recorder_tests;

pub struct RtpTrackBuilder {
    track_id: String,
    cancel_token: Option<CancellationToken>,
    external_ip: Option<String>,
    rtp_start_port: Option<u16>,
    rtp_end_port: Option<u16>,
    mode: TransportMode,
    rtp_map: Vec<negotiate::CodecInfo>,
    enable_latching: bool,
    ice_servers: Vec<IceServer>,
}

impl RtpTrackBuilder {
    pub fn new(track_id: String) -> Self {
        Self {
            track_id,
            cancel_token: None,
            external_ip: None,
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
                channels: c.channels() as u16,
                codec: c,
            })
            .collect(),
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

    pub fn with_codec_preference(mut self, codecs: Vec<CodecType>) -> Self {
        self.rtp_map = codecs
            .into_iter()
            .map(|c| negotiate::CodecInfo {
                payload_type: c.payload_type(),
                clock_rate: c.clock_rate(),
                channels: c.channels() as u16,
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

    pub fn build(self) -> RtcTrack {
        let config = RtcConfiguration {
            ice_servers: self.ice_servers,
            transport_mode: self.mode,
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
            external_ip: self.external_ip,
            enable_latching: self.enable_latching,
            ..Default::default()
        };

        RtcTrack::new(self.track_id, config, self.rtp_map)
    }
}

/// Audio file playback track with loop support
///
/// Used for playing audio files (e.g., ringback tones, hold music, announcements).
#[derive(Clone)]
pub struct FileTrack {
    track_id: String,
    file_path: Option<String>,
    loop_playback: bool,
    cancel_token: CancellationToken,
    pc: PeerConnection,
    completion_notify: Arc<tokio::sync::Notify>,
    codec_preference: Vec<CodecType>,
    mode: TransportMode,
    rtp_start_port: Option<u16>,
    rtp_end_port: Option<u16>,
    external_ip: Option<String>,
    audio_source_manager: Option<Arc<audio_source::AudioSourceManager>>,
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
            mode: TransportMode::Rtp,
            rtp_start_port: None,
            rtp_end_port: None,
            external_ip: None,
            audio_source_manager: None,
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

    fn recreate_pc(&mut self) {
        let config = RtcConfiguration {
            transport_mode: self.mode.clone(),
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
            external_ip: self.external_ip.clone(),
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
            .codec_preference
            .first()
            .map(|c| c.clock_rate())
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

    /// Start audio playback task
    ///
    pub async fn start_playback(&self) -> Result<()> {
        let file_path = self
            .file_path
            .as_ref()
            .ok_or_else(|| anyhow!("No file path set"))?;

        if !std::path::Path::new(file_path).exists() {
            return Err(anyhow!("Audio file not found: {}", file_path));
        }

        debug!(
            file = %file_path,
            loop_playback = self.loop_playback,
            "FileTrack playback started"
        );

        let completion_notify = self.completion_notify.clone();
        let loop_playback = self.loop_playback;
        let cancel_token = self.cancel_token.clone();

        crate::utils::spawn(async move {
            if !loop_playback {
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                        debug!("FileTrack playback completed");
                    }
                    _ = cancel_token.cancelled() => {
                        debug!("FileTrack playback cancelled");
                    }
                }
                completion_notify.notify_waiters();
            } else {
                cancel_token.cancelled().await;
                debug!("FileTrack playback stopped");
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

        if !self.codec_preference.is_empty() {
            if let Some(section) = offer
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
}

#[cfg(test)]
mod media_track_tests;
