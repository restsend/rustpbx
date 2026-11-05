//! Recording backends: MP4 per-leg video recorder + composite fan-out.

use std::collections::VecDeque;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rustrtc::media::{Depacketizer, H264Depacketizer, MediaKind};
use rustrtc::rtp::RtpPacket;
use tracing::{trace, warn};

use crate::media_recorder::{
    CaptureChannel, MediaRecorder, RecordingResult, RecorderStatus,
};
use crate::negotiate::NegotiatedLegProfile;

use super::assembler::{H264FrameAssembler, VideoFrame};
use super::muxer::{Mp4Artifact, Mp4Muxer, ensure_parent_dir};

/// Options for [`Mp4Recorder`].
#[derive(Debug, Clone)]
pub struct Mp4RecorderOptions {
    /// Output path *without* extension, e.g.
    /// `{recordings}/{session}_{seq}_{label}` — each leg appends its
    /// channel suffix (`..._caller.mp4` / `..._callee.mp4`).
    pub base_path: String,
    /// Log once per stream when a non-H264 video codec is negotiated
    /// (only H264 is muxed today).
    pub warn_on_unsupported_codec: bool,
}

impl Default for Mp4RecorderOptions {
    fn default() -> Self {
        Self {
            base_path: String::new(),
            warn_on_unsupported_codec: true,
        }
    }
}

/// Per-leg muxer state. Files are created lazily on the first aligned
/// keyframe, so a video-less leg never produces an empty artifact.
#[derive(Default)]
struct LegMuxState {
    /// FU-A/STAP-A reassembly is stream state — one depacketizer per leg.
    depacketizer: Option<H264Depacketizer>,
    assembler: H264FrameAssembler,
    /// Frames buffered while waiting for the first keyframe; dropped
    /// entirely once the keyframe arrives (nothing may precede an IDR in a
    /// decodable file, and mid-call starts must not block capture).
    pending: VecDeque<VideoFrame>,
    muxer: Option<Mp4Muxer>,
    /// Wall-clock (unix micros) of the first written frame.
    started_wall_micros: u64,
    path: Option<String>,
    frame_count: u64,
    /// Set once a non-H264 video packet has been warned about.
    warned_unsupported: bool,
}

impl LegMuxState {
    fn new() -> Self {
        Self {
            depacketizer: Some(H264Depacketizer::new()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoPacketKind {
    /// Audio/DTMF (owned by the WAV artifact in a composite).
    Audio,
    /// H264 video — muxed.
    H264,
    /// A negotiated video codec this backend cannot mux (VP8 today).
    OtherVideo,
    /// Not part of this leg's negotiation.
    Unknown,
}

/// Per-leg MP4 video recorder: caller and callee files contain the media
/// that leg *sent*, mirroring the stereo WAV convention (each side on its
/// own channel). No audio track: the WAV artifact covers audio (no pure-Rust
/// Opus-capable MP4 muxer exists today).
pub struct Mp4Recorder {
    options: Mp4RecorderOptions,
    legs: [LegMuxState; 2],
}

fn channel_index(channel: CaptureChannel) -> usize {
    match channel {
        CaptureChannel::Caller => 0,
        CaptureChannel::Callee => 1,
    }
}

impl Mp4Recorder {
    pub fn new(options: Mp4RecorderOptions) -> Self {
        Self {
            options,
            legs: [LegMuxState::new(), LegMuxState::new()],
        }
    }

    /// Classify a captured packet for this backend: audio/DTMF rides the
    /// composite too (the WAV covers it — silently ignored here), H264 video
    /// is muxed, other video codecs warn once, anything unknown is skipped.
    fn classify(
        &self,
        profile: Option<&NegotiatedLegProfile>,
        pt: u8,
    ) -> VideoPacketKind {
        let Some(profile) = profile else {
            // No profile: the session-level guard already ensured the call
            // negotiated H264; let NAL validation handle the payload.
            return VideoPacketKind::H264;
        };
        if profile.audio.as_ref().is_some_and(|a| a.payload_type == pt)
            || profile.dtmf_pts.contains(&pt)
        {
            return VideoPacketKind::Audio;
        }
        match profile.video.iter().find(|codec| codec.payload_type == pt) {
            None => VideoPacketKind::Unknown,
            Some(codec) if codec.name.eq_ignore_ascii_case("H264") => VideoPacketKind::H264,
            Some(_) => VideoPacketKind::OtherVideo,
        }
    }

    /// Feed one depacketized frame into the leg's assembler/muxer.
    async fn handle_frame(&mut self, index: usize, frame: VideoFrame) {
        let state = &mut self.legs[index];
        if !state.assembler.has_decoder_config() {
            // Keep buffering until SPS/PPS + IDR are known; hard cap so a
            // pathological stream (no config ever) cannot grow unbounded.
            if state.pending.len() < 600 {
                state.pending.push_back(frame);
            } else if let Some(oldest) = state.pending.pop_front() {
                state.pending.push_back(frame);
                let _ = oldest;
            }
            return;
        }
        if !frame.is_keyframe && state.muxer.is_none() {
            // Config known but no keyframe yet: keep buffering (bounded).
            if state.pending.len() < 600 {
                state.pending.push_back(frame);
            }
            return;
        }
        if state.muxer.is_none() {
            // First decodable point: SPS/PPS + IDR. Discard pre-IDR frames —
            // nothing before an IDR is decodable, so pending is dropped.
            state.pending.clear();
            let base = &self.options.base_path;
            let suffix = if index == 0 { "caller" } else { "callee" };
            let path = std::path::PathBuf::from(format!("{base}_{suffix}.mp4"));
            if let Err(error) = ensure_parent_dir(&path).await {
                warn!(path = %path.display(), %error, "mp4 recorder parent dir failed");
                return;
            }
            let (sps, pps) = (
                state.assembler.sps().unwrap_or_default().to_vec(),
                state.assembler.pps().unwrap_or_default().to_vec(),
            );
            state.started_wall_micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or_default();
            match Mp4Muxer::create(&path, &sps, &pps, state.started_wall_micros) {
                Ok(muxer) => {
                    state.path = Some(path.display().to_string());
                    state.muxer = Some(muxer);
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "mp4 recorder create failed");
                    return;
                }
            }
        }
        if let Some(muxer) = state.muxer.as_mut()
            && let Err(error) = muxer.write_frame(&frame)
        {
            warn!(%error, "mp4 recorder write failed");
        } else {
            trace!(
                leg = index,
                ts = frame.rtp_timestamp,
                keyframe = frame.is_keyframe,
                bytes = frame.data.len(),
                "mp4 recorder: frame written"
            );
            state.frame_count += 1;
        }
    }
}

#[async_trait]
impl MediaRecorder for Mp4Recorder {
    async fn initialize(&mut self) -> Result<()> {
        if self.options.base_path.is_empty() {
            return Err(anyhow!("mp4 recorder has no base path"));
        }
        // Parent dir pre-created so per-leg lazy file creation cannot race
        // the date subdir cleanup.
        ensure_parent_dir(&std::path::PathBuf::from(format!(
            "{}_caller.mp4",
            self.options.base_path
        )))
        .await
    }

    async fn write_rtp_with_profile(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        _received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
        profile: Option<&NegotiatedLegProfile>,
    ) -> Result<()> {
        let index = channel_index(channel);
        let pt = packet.header.payload_type;
        match self.classify(profile, pt) {
            VideoPacketKind::Audio | VideoPacketKind::Unknown => return Ok(()),
            VideoPacketKind::OtherVideo => {
                if !self.legs[index].warned_unsupported {
                    self.legs[index].warned_unsupported = true;
                    if self.options.warn_on_unsupported_codec {
                        warn!(
                            pt,
                            "video recording: non-H264 video codec not muxed (MP4 backend)"
                        );
                    }
                }
                return Ok(());
            }
            VideoPacketKind::H264 => {}
        }
        // Address is only carried for diagnostics inside rustrtc.
        let addr = peer_addr.unwrap_or_else(|| "0.0.0.0:0".parse().expect("static addr"));
        let Some(depacketizer) = self.legs[index].depacketizer.as_mut() else {
            return Ok(());
        };
        let samples = depacketizer
            .push(packet.clone(), 90_000, addr, MediaKind::Video)
            .map_err(|error| anyhow!("video depacketizer failed: {error}"))?;
        for sample in &samples {
            if let Some(frame) = self.legs[index].assembler.push_media_sample(sample) {
                self.handle_frame(index, frame).await;
            }
        }
        Ok(())
    }

    async fn write_rtp(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<()> {
        self.write_rtp_with_profile(
            channel,
            packet,
            received_at_micros,
            peer_addr,
            None,
        )
        .await
    }

    /// The caller artifact is the primary result; the callee file rides as
    /// an extra so CDR registration lists both.
    async fn finalize(mut self: Box<Self>) -> Result<Option<RecordingResult>> {
        let mut artifacts: Vec<(usize, Mp4Artifact)> = Vec::new();
        for index in 0..self.legs.len() {
            let state = &mut self.legs[index];
            // Flush a trailing partial access unit before closing.
            if let Some(muxer) = state.muxer.as_mut()
                && let Some(frame) = state.assembler.flush()
                && let Err(error) = muxer.write_frame(&frame)
            {
                warn!(%error, "mp4 recorder final frame write failed");
            }
            if let Some(muxer) = state.muxer.take() {
                match muxer.finalize().await {
                    Ok(artifact) => artifacts.push((index, artifact)),
                    Err(error) => warn!(%error, "mp4 recorder finalize failed"),
                }
            }
        }
        if artifacts.is_empty() {
            return Ok(None);
        }
        artifacts.sort_by_key(|(index, _)| *index);
        let to_result = |artifact: &Mp4Artifact| RecordingResult {
            path: artifact.path.clone(),
            duration_secs: artifact.duration_secs,
            file_size: artifact.file_size,
            extra_files: Vec::new(),
        };
        let (first_index, first_artifact) = artifacts.remove(0);
        let mut result = to_result(&first_artifact);
        if first_index == 0 && let Some((_, callee)) = artifacts.first() {
            result.extra_files.push(to_result(callee));
        }
        Ok(Some(result))
    }
}

/// Combines several backends (e.g. WAV + MP4) inside ONE recording task.
/// Initialization/writes fan out sequentially; finalize merges every
/// backend's artifact list under the first file result so the session's
/// publish path stays unchanged.
pub struct CompositeMediaRecorder {
    backends: Vec<Box<dyn MediaRecorder>>,
}

impl CompositeMediaRecorder {
    pub fn new(backends: Vec<Box<dyn MediaRecorder>>) -> Self {
        Self { backends }
    }
}

/// Recursively flatten a finalize result (and any nested extras) into a
/// flat artifact list — the primary path first, nested extras after it.
fn flatten_result(result: RecordingResult, out: &mut Vec<RecordingResult>) {
    let RecordingResult {
        path,
        duration_secs,
        file_size,
        extra_files,
    } = result;
    out.push(RecordingResult {
        path,
        duration_secs,
        file_size,
        extra_files: Vec::new(),
    });
    for nested in extra_files {
        flatten_result(nested, out);
    }
}

#[async_trait]
impl MediaRecorder for CompositeMediaRecorder {
    fn file_path(&self) -> Option<&str> {
        self.backends.iter().find_map(|backend| backend.file_path())
    }

    fn resume(&mut self) {
        for backend in self.backends.iter_mut() {
            backend.resume();
        }
    }

    async fn initialize(&mut self) -> Result<()> {
        for backend in self.backends.iter_mut() {
            backend.initialize().await?;
        }
        Ok(())
    }

    async fn write_rtp_with_profile(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
        profile: Option<&NegotiatedLegProfile>,
    ) -> Result<()> {
        for backend in self.backends.iter_mut() {
            backend
                .write_rtp_with_profile(
                    channel,
                    packet,
                    received_at_micros,
                    peer_addr,
                    profile,
                )
                .await?;
        }
        Ok(())
    }

    async fn write_rtp(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<()> {
        for backend in self.backends.iter_mut() {
            backend
                .write_rtp(channel, packet, received_at_micros, peer_addr)
                .await?;
        }
        Ok(())
    }

    async fn finalize(mut self: Box<Self>) -> Result<Option<RecordingResult>> {
        // Flatten: a child backend (e.g. the MP4 recorder) may itself produce
        // per-leg extras — every artifact surfaces at the top level, primary
        // first (first backend with an artifact).
        let mut flattened: Vec<RecordingResult> = Vec::new();
        for backend in self.backends.into_iter() {
            if let Some(result) = backend.finalize().await? {
                flatten_result(result, &mut flattened);
            }
        }
        let mut iter = flattened.into_iter();
        let Some(mut merged) = iter.next() else {
            return Ok(None);
        };
        merged.extra_files.extend(iter);
        Ok(Some(merged))
    }
}

/// Convenience for status reporting (used by session log lines).
pub fn describe_status(status: &RecorderStatus) -> String {
    match (&status.file_path, status.active) {
        (Some(path), _) => path.clone(),
        (None, true) => "active".to_string(),
        (None, false) => "idle".to_string(),
    }
}

/// Helper for tests: count of legs with an open muxer.
#[cfg(test)]
impl Mp4Recorder {
    pub(crate) fn open_legs(&self) -> usize {
        self.legs
            .iter()
            .filter(|leg| leg.muxer.is_some())
            .count()
    }

    pub(crate) fn pending_frames(&self, index: usize) -> usize {
        self.legs[index].pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_recorder::RecordingResult;
    use crate::negotiate::NegotiatedVideoCodec;
    use rustrtc::rtp::RtpHeader;

    fn h264_profile(pt: u8, name: &str) -> NegotiatedLegProfile {
        NegotiatedLegProfile {
            video: vec![NegotiatedVideoCodec {
                name: name.to_string(),
                payload_type: pt,
                clock_rate: 90_000,
                fmtp: None,
                rtcp_fbs: Vec::new(),
                rtx_payload_type: None,
            }],
            ..Default::default()
        }
    }

    /// One single-NALU H264 RTP packet (marker set: the whole frame).
    fn video_packet(pt: u8, seq: u16, ts: u32, nal_type: u8, body: &[u8]) -> RtpPacket {
        let mut header = RtpHeader::new(pt, seq, ts, 4242);
        header.marker = true;
        let mut payload = vec![0x60 | nal_type];
        payload.extend_from_slice(body);
        RtpPacket::new(header, payload)
    }

    fn sps_nal() -> Vec<u8> {
        crate::video_recorder::sps::fixtures::baseline_sps(66, 40, 30, false)
    }

    fn pps_nal() -> Vec<u8> {
        vec![0x68, 0xCE, 0x3C, 0x80]
    }

    #[tokio::test]
    async fn mp4_recorder_writes_per_leg_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("sess01_1_full");
        let options = Mp4RecorderOptions {
            base_path: base.display().to_string(),
            ..Default::default()
        };
        let mut recorder = Mp4Recorder::new(options);
        recorder.initialize().await.expect("initialize");

        let profile = h264_profile(96, "H264");
        // Caller leg: SPS + PPS + IDR, then two inter frames.
        let caller_stream: Vec<(u16, u32, u8, Vec<u8>)> = vec![
            (1, 3000, 7, sps_nal()),
            (2, 3000, 8, pps_nal()),
            (3, 3000, 5, b"idr1".to_vec()),
            (4, 6000, 1, b"p1".to_vec()),
            (5, 9000, 1, b"p2".to_vec()),
        ];
        for (seq, ts, nal_type, body) in caller_stream {
            let packet = video_packet(96, seq, ts, nal_type, &body);
            recorder
                .write_rtp_with_profile(
                    CaptureChannel::Caller,
                    &packet,
                    0,
                    None,
                    Some(&profile),
                )
                .await
                .expect("caller write");
        }
        // Callee leg: its own IDR + one inter frame.
        let callee_stream: Vec<(u16, u32, u8, Vec<u8>)> = vec![
            (1, 3000, 7, sps_nal()),
            (2, 3000, 8, pps_nal()),
            (3, 3000, 5, b"idr1".to_vec()),
            (4, 6000, 1, b"p1".to_vec()),
        ];
        for (seq, ts, nal_type, body) in callee_stream {
            let packet = video_packet(96, seq, ts, nal_type, &body);
            recorder
                .write_rtp_with_profile(
                    CaptureChannel::Callee,
                    &packet,
                    0,
                    None,
                    Some(&profile),
                )
                .await
                .expect("callee write");
        }

        let result: Option<RecordingResult> = Box::new(recorder).finalize().await.expect("finalize");
        let result = result.expect("artifacts produced");
        assert_eq!(result.path, format!("{}_caller.mp4", base.display()));
        assert_eq!(result.extra_files.len(), 1, "callee artifact as extra");
        assert_eq!(result.extra_files[0].path, format!("{}_callee.mp4", base.display()));

        // Both files parse back as MP4 with the expected sample counts.
        for (path, samples) in [
            (result.path.as_str(), 3u32),
            (result.extra_files[0].path.as_str(), 2u32),
        ] {
            let file = std::fs::File::open(path).expect("open");
            let mut reader = mp4::read_mp4(file).expect("parse");
            assert_eq!(reader.sample_count(1).expect("count"), samples, "{path}");
            let first = reader.read_sample(1, 1).expect("read").expect("present");
            assert!(first.is_sync, "file starts at IDR: {path}");
        }
    }

    #[tokio::test]
    async fn pre_idr_frames_dropped_and_recording_starts_at_keyframe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("sess02_1_full");
        let mut recorder = Mp4Recorder::new(Mp4RecorderOptions {
            base_path: base.display().to_string(),
            ..Default::default()
        });
        recorder.initialize().await.expect("initialize");
        let profile = h264_profile(96, "H264");

        // P frame BEFORE any config/keyframe: buffered, then dropped when the
        // IDR arrives — a file starting on a non-keyframe is undecodable.
        let p_early = video_packet(96, 1, 3000, 1, b"early-p");
        recorder
            .write_rtp_with_profile(CaptureChannel::Caller, &p_early, 0, None, Some(&profile))
            .await
            .expect("early p");
        assert_eq!(recorder.open_legs(), 0, "no muxer before keyframe");
        assert_eq!(recorder.pending_frames(0), 1);

        let idr_stream: Vec<(u16, u32, u8, Vec<u8>)> = vec![
            (2, 6000, 7, sps_nal()),
            (3, 6000, 8, pps_nal()),
            (4, 6000, 5, b"idr".to_vec()),
            (5, 9000, 1, b"p1".to_vec()),
        ];
        for (seq, ts, nal_type, body) in idr_stream {
            let packet = video_packet(96, seq, ts, nal_type, &body);
            recorder
                .write_rtp_with_profile(CaptureChannel::Caller, &packet, 0, None, Some(&profile))
                .await
                .expect("idr stream");
        }
        assert_eq!(recorder.pending_frames(0), 0, "pending dropped on IDR");

        let result = Box::new(recorder).finalize().await.expect("finalize").expect("artifact");
        let file = std::fs::File::open(&result.path).expect("open");
        let reader = mp4::read_mp4(file).expect("parse");
        // IDR + one inter frame; the pre-IDR P frame never made it in.
        assert_eq!(reader.sample_count(1).expect("count"), 2);
    }

    #[tokio::test]
    async fn non_h264_video_codec_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("sess03_1_full");
        let mut recorder = Mp4Recorder::new(Mp4RecorderOptions {
            base_path: base.display().to_string(),
            ..Default::default()
        });
        recorder.initialize().await.expect("initialize");
        let profile = h264_profile(98, "VP8");
        let packet = video_packet(98, 1, 3000, 5, b"idr");
        recorder
            .write_rtp_with_profile(CaptureChannel::Caller, &packet, 0, None, Some(&profile))
            .await
            .expect("write");
        let result = Box::new(recorder).finalize().await.expect("finalize");
        assert!(result.is_none(), "VP8 stream produces no artifact");
    }
}
