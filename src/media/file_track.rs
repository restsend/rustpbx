use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use audio_codec::create_encoder;
use rustrtc::{
    Attribute, MediaKind, PeerConnection, RtcConfiguration, RtpCodecParameters, SdpType,
    SessionDescription, TransceiverDirection, TransportMode,
    config::BufferDropStrategy,
    media::{AudioFrame, MediaSample},
};
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::media::Track;
use crate::media::audio_source;
use crate::media::negotiate;

// ---------------------------------------------------------------------------
// PlaybackEndReason / PlaybackEndCallback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackEndReason {
    Completed,
    Interrupted,
}

pub type PlaybackEndCallback = Arc<dyn Fn(PlaybackEndReason) + Send + Sync + 'static>;

// ---------------------------------------------------------------------------
// AudioFrameTiming helper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AudioFrameTiming {
    pub pcm_sample_rate: u32,
    pub pcm_samples_per_frame: usize,
    pub rtp_ticks_per_frame: u32,
}

pub(crate) fn audio_frame_timing(codec: CodecType, rtp_clock_rate: u32) -> AudioFrameTiming {
    let frame_ms = 20u32;
    let pcm_sample_rate = codec.samplerate();

    AudioFrameTiming {
        pcm_sample_rate,
        pcm_samples_per_frame: (pcm_sample_rate * frame_ms / 1000) as usize,
        rtp_ticks_per_frame: rtp_clock_rate * frame_ms / 1000,
    }
}

// ---------------------------------------------------------------------------
// FileTrack
// ---------------------------------------------------------------------------

/// Audio file playback track with loop support
///
/// Used for playing audio files (e.g., ringback tones, hold music, announcements).
pub struct FileTrack {
    pub(crate) track_id: String,
    pub(crate) session_id: Option<String>,
    pub(crate) file_path: Option<String>,
    pub(crate) loop_playback: bool,
    cancel_token: CancellationToken,
    pc: PeerConnection,
    on_end: Option<PlaybackEndCallback>,
    pub(crate) codec_preference: Vec<CodecType>,
    codec_info: Option<negotiate::CodecInfo>,
    mode: TransportMode,
    rtp_start_port: Option<u16>,
    rtp_end_port: Option<u16>,
    external_ip: Option<String>,
    bind_ip: Option<String>,
    audio_source_manager: Option<Arc<audio_source::AudioSourceManager>>,
    muted: std::sync::atomic::AtomicBool,
    cname: Option<String>,
}

impl Clone for FileTrack {
    fn clone(&self) -> Self {
        Self {
            track_id: self.track_id.clone(),
            session_id: self.session_id.clone(),
            file_path: self.file_path.clone(),
            loop_playback: self.loop_playback,
            cancel_token: self.cancel_token.clone(),
            pc: self.pc.clone(),
            on_end: self.on_end.clone(),
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
            cname: self.cname.clone(),
        }
    }
}

impl FileTrack {
    pub fn new(track_id: String) -> Self {
        let config = RtcConfiguration {
            transport_mode: TransportMode::Rtp,
            buffer_drop_strategy: BufferDropStrategy::DropOldest,
            ..Default::default()
        };

        let pc = {
            let _guard = crate::utils::media_enter();
            let pc = PeerConnection::new(config);
            pc.add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly);
            pc
        };

        Self {
            session_id: None,
            track_id,
            file_path: None,
            loop_playback: false,
            cancel_token: CancellationToken::new(),
            pc,
            on_end: None,
            codec_preference: vec![CodecType::PCMU, CodecType::PCMA],
            codec_info: None,
            mode: TransportMode::Rtp,
            rtp_start_port: None,
            rtp_end_port: None,
            external_ip: None,
            bind_ip: None,
            audio_source_manager: None,
            muted: std::sync::atomic::AtomicBool::new(false),
            cname: None,
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

    pub fn with_on_end(mut self, on_end: PlaybackEndCallback) -> Self {
        self.on_end = Some(on_end);
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

    pub fn with_cname(mut self, cname: String) -> Self {
        self.cname = Some(cname);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn codec_info(&self) -> Option<&negotiate::CodecInfo> {
        self.codec_info.as_ref()
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
            cname: self.cname.clone(),
            buffer_drop_strategy: BufferDropStrategy::DropOldest,
            ..Default::default()
        };

        let _guard = crate::utils::media_enter();
        self.pc = PeerConnection::new(config);
        self.pc
            .add_transceiver(MediaKind::Audio, TransceiverDirection::SendOnly);
    }

    async fn init_audio_source(&mut self) -> Result<()> {
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
            manager
                .switch_to_file(path.clone(), self.loop_playback)
                .await?;
        } else {
            manager.switch_to_silence();
        }

        self.audio_source_manager = Some(manager);
        Ok(())
    }

    pub async fn start_playback(&self) -> Result<()> {
        self.start_playback_on(None).await
    }

    pub(crate) async fn create_playback_source(&self) -> Result<FileTrackPlaybackSource> {
        let file_path = self.file_path.as_deref();

        if let Some(file_path) = file_path {
            let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
            if !is_remote && !std::path::Path::new(file_path).exists() {
                return Err(anyhow!("Audio file not found: {}", file_path));
            }
        }

        let selected = self.codec_info.clone().unwrap_or_else(|| {
            let codec = self
                .codec_preference
                .first()
                .copied()
                .unwrap_or(CodecType::PCMU);
            negotiate::MediaNegotiator::codec_info_for_type(codec)
        });
        let frame_timing = audio_frame_timing(selected.codec, selected.clock_rate);
        let audio_source_manager = {
            if let Some(ref mgr) = self.audio_source_manager {
                mgr.clone()
            } else {
                let mgr = Arc::new(audio_source::AudioSourceManager::new(
                    frame_timing.pcm_sample_rate,
                ));
                if let Some(file_path) = file_path {
                    mgr.switch_to_file(file_path.to_string(), self.loop_playback)
                        .await?;
                } else {
                    mgr.switch_to_silence();
                }
                mgr
            }
        };

        debug!(
            track_id = %self.track_id,
            session_id = ?self.session_id,
            file = %file_path.unwrap_or("<silence>"),
            loop_playback = self.loop_playback,
            codec = ?selected.codec,
            samples_per_frame = frame_timing.pcm_samples_per_frame,
            pcm_sample_rate = frame_timing.pcm_sample_rate,
            rtp_ticks_per_frame = frame_timing.rtp_ticks_per_frame,
            "FileTrack playback source created"
        );

        Ok(FileTrackPlaybackSource {
            audio_source_manager,
            encoder: create_encoder(selected.codec),
            codec_info: selected,
            rtp_ticks_per_frame: frame_timing.rtp_ticks_per_frame,
            rtp_timestamp: rand::random(),
            sequence_number: rand::random(),
            on_end: self.on_end.clone(),
            loop_playback: self.loop_playback,
            pcm_buf: vec![0i16; frame_timing.pcm_samples_per_frame],
        })
    }

    pub async fn start_playback_on(&self, target_pc: Option<PeerConnection>) -> Result<()> {
        use audio_codec::create_encoder;
        use rustrtc::media::{AudioFrame, MediaSample};

        let file_path = self
            .file_path
            .as_ref()
            .ok_or_else(|| anyhow!("No file path set"))?;

        let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
        if !is_remote && !std::path::Path::new(file_path).exists() {
            return Err(anyhow!("Audio file not found: {}", file_path));
        }

        let selected = self.codec_info.clone().unwrap_or_else(|| {
            let codec = self
                .codec_preference
                .first()
                .copied()
                .unwrap_or(CodecType::PCMU);
            negotiate::MediaNegotiator::codec_info_for_type(codec)
        });
        let codec = selected.codec;
        let payload_type = selected.payload_type;
        let frame_timing = audio_frame_timing(codec, selected.clock_rate);
        let samples_per_frame = frame_timing.pcm_samples_per_frame;

        let has_external_pc = target_pc.is_some();
        let pc = target_pc.unwrap_or_else(|| self.pc.clone());

        debug!(
            track_id = %self.track_id,
            session_id = ?self.session_id,
            file = %file_path,
            loop_playback = self.loop_playback,
            ?codec,
            samples_per_frame,
            pcm_sample_rate = frame_timing.pcm_sample_rate,
            rtp_ticks_per_frame = frame_timing.rtp_ticks_per_frame,
            has_external_pc,
            "FileTrack start_playback_on"
        );

        let audio_source_manager = {
            if let Some(ref mgr) = self.audio_source_manager {
                mgr.clone()
            } else {
                let mgr = Arc::new(audio_source::AudioSourceManager::new(
                    frame_timing.pcm_sample_rate,
                ));
                mgr.switch_to_file(file_path.clone(), self.loop_playback)
                    .await?;
                mgr
            }
        };

        let (source_target, track_target, _) =
            rustrtc::media::track::sample_track(rustrtc::media::MediaKind::Audio, 100);

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
            let mut builder = rustrtc::RtpSender::builder(track_arc, ssrc).params(params);
            if let Some(ref cname) = self.cname {
                builder = builder.cname(cname.clone());
            }
            let new_sender = builder.build();
            transceiver.set_sender(Some(new_sender));
        } else {
            let _ = pc.add_track(track_target, params);
        }

        let mut on_end = self.on_end.clone();
        let cancel_token = self.cancel_token.clone();
        let loop_playback = self.loop_playback;

        crate::utils::media_spawn(async move {
            let mut encoder = create_encoder(codec);
            let mut rtp_timestamp: u32 = rand::random();
            let mut sequence_number: u16 = rand::random();
            let interval_ms = 20u64;
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut pcm_buf: Vec<i16> = vec![0i16; samples_per_frame];

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!("FileTrack playback cancelled");
                        if let Some(on_end) = on_end.take() {
                            on_end(PlaybackEndReason::Interrupted);
                        }
                        break;
                    }
                    _ = interval.tick() => {
                        let read = audio_source_manager.read_samples(&mut pcm_buf);

                        if read == 0 {
                            if !loop_playback {
                                let reason = if cancel_token.is_cancelled() {
                                    PlaybackEndReason::Interrupted
                                } else {
                                    PlaybackEndReason::Completed
                                };
                                debug!("FileTrack playback completed (file exhausted)");
                                if let Some(on_end) = on_end.take() {
                                    on_end(reason);
                                }
                                break;
                            }
                            continue;
                        }

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

                        if let Err(e) = source_target.send(MediaSample::Audio(frame)) {
                            debug!("FileTrack source_target.send failed (receiver gone): {}", e);
                            if let Some(on_end) = on_end.take() {
                                on_end(PlaybackEndReason::Interrupted);
                            }
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn switch_audio_source(
        &mut self,
        file_path: String,
        loop_playback: bool,
    ) -> Result<()> {
        if self.audio_source_manager.is_none() {
            self.init_audio_source().await?;
        }

        if let Some(ref manager) = self.audio_source_manager {
            manager.switch_to_file(file_path, loop_playback).await?;
        }

        Ok(())
    }

    pub fn switch_to_silence(&mut self) {
        if let Some(ref manager) = self.audio_source_manager {
            manager.switch_to_silence();
        }
    }
}

// ---------------------------------------------------------------------------
// FileTrackPlaybackSource
// ---------------------------------------------------------------------------

pub(crate) struct FileTrackPlaybackSource {
    audio_source_manager: Arc<audio_source::AudioSourceManager>,
    encoder: Box<dyn audio_codec::Encoder>,
    codec_info: negotiate::CodecInfo,
    rtp_ticks_per_frame: u32,
    rtp_timestamp: u32,
    sequence_number: u16,
    on_end: Option<PlaybackEndCallback>,
    loop_playback: bool,
    pcm_buf: Vec<i16>,
}

impl FileTrackPlaybackSource {
    pub(crate) fn next_audio_sample(&mut self) -> Option<MediaSample> {
        let read = {
            let buf = &mut self.pcm_buf;
            let mut read = self.audio_source_manager.read_samples(buf);
            if read == 0 && self.loop_playback {
                read = self.audio_source_manager.read_samples(buf);
            }
            read
        };

        if read == 0 {
            debug!("FileTrack playback completed (source exhausted)");
            if let Some(on_end) = self.on_end.take() {
                on_end(PlaybackEndReason::Completed);
            }
            return None;
        }

        let encoded = self.encoder.encode(&self.pcm_buf[..read]);
        let frame = AudioFrame {
            rtp_timestamp: self.rtp_timestamp,
            clock_rate: self.codec_info.clock_rate,
            data: encoded.into(),
            sequence_number: Some(self.sequence_number),
            payload_type: Some(self.codec_info.payload_type),
            marker: false,
            header_extension: None,
            raw_packet: None,
            source_addr: None,
        };

        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(self.rtp_ticks_per_frame);
        self.sequence_number = self.sequence_number.wrapping_add(1);

        Some(MediaSample::Audio(frame))
    }
}

impl Drop for FileTrackPlaybackSource {
    fn drop(&mut self) {
        if let Some(on_end) = self.on_end.take() {
            on_end(PlaybackEndReason::Interrupted);
        }
    }
}

// ---------------------------------------------------------------------------
// Track impl for FileTrack
// ---------------------------------------------------------------------------

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
        // `PeerConnection::close()` is idempotent — safe to call even if
        // another clone already closed it.
        self.pc.close();
    }
}
