//! MP4 (ISOBMFF) H264 file writer on top of the `mp4` crate.
//!
//! One video track (AVCC), 90 kHz timescale, samples written as frames
//! arrive; `write_end` emits the `moov` on finalization so the file plays
//! in any ISO-BMFF player after a clean stop.
//!
//! The recording task owns the muxer serially, so blocking file I/O here is
//! on the recording runtime — never on the RTP hot path.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use mp4::{AvcConfig, FourCC, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

use super::assembler::VideoFrame;
use super::sps;

/// RTP clock rate of the H264 track (RFC 6184).
pub const VIDEO_CLOCK_RATE: u32 = 90_000;
/// Fallback frame duration when the RTP timestamp gives no usable delta
/// (30 fps ≙ 3000 ticks).
const DEFAULT_FRAME_TICKS: u32 = 3_000;
/// One 90 kHz second — deltas beyond this indicate a stream discontinuity.
const MAX_FRAME_TICKS: u32 = 2 * VIDEO_CLOCK_RATE;

/// A finalized MP4 artifact.
#[derive(Debug, Clone)]
pub struct Mp4Artifact {
    pub path: String,
    pub duration_secs: f64,
    pub file_size: u64,
    pub frame_count: u64,
}

/// Serial MP4 writer for one recorded leg.
pub struct Mp4Muxer {
    writer: Mp4Writer<std::fs::File>,
    path: PathBuf,
    /// RTP timestamp of the previously written frame (duration derivation).
    prev_ts: Option<u32>,
    /// Duration applied to the last sample (used as fallback).
    last_duration: u32,
    /// Cumulative start time of the next sample in track ticks.
    next_start: u64,
    frame_count: u64,
    started_wall_micros: u64,
}

impl Mp4Muxer {
    /// Create the file and add the H264 track from captured SPS/PPS.
    ///
    /// Dimensions come from the SPS geometry when parseable; the fallback
    /// keeps the file structurally valid (players read the true geometry
    /// from `avcC`).
    pub fn create(
        path: impl Into<PathBuf>,
        sps_nal: &[u8],
        pps_nal: &[u8],
        started_wall_micros: u64,
    ) -> Result<Self> {
        let path = path.into();
        let geometry = sps::parse_sps_geometry(sps_nal);
        let (width, height) = geometry
            .map(|geo| (geo.width, geo.height))
            .unwrap_or((640, 480));
        if geometry.is_none() {
            tracing::warn!(
                path = %path.display(),
                "SPS geometry parse failed; using default 640x480 in tkhd"
            );
        }
        let file = std::fs::File::create(&path)
            .map_err(|error| anyhow!("failed to create {}: {error}", path.display()))?;
        let brand = |name: &[u8; 4]| -> FourCC { FourCC { value: *name } };
        let config = Mp4Config {
            major_brand: brand(b"isom"),
            minor_version: 512,
            compatible_brands: vec![
                brand(b"isom"),
                brand(b"iso2"),
                brand(b"avc1"),
                brand(b"mp41"),
            ],
            timescale: VIDEO_CLOCK_RATE,
        };
        let mut writer = Mp4Writer::write_start(file, &config)?;
        writer.add_track(&TrackConfig {
            track_type: TrackType::Video,
            timescale: VIDEO_CLOCK_RATE,
            language: "und".to_string(),
            media_conf: MediaConfig::AvcConfig(AvcConfig {
                width,
                height,
                seq_param_set: sps_nal.to_vec(),
                pic_param_set: pps_nal.to_vec(),
            }),
        })?;
        Ok(Self {
            writer,
            path,
            prev_ts: None,
            last_duration: DEFAULT_FRAME_TICKS,
            next_start: 0,
            frame_count: 0,
            started_wall_micros,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Write one assembled frame. Timestamps drive sample durations; gaps
    /// and jitter clamp to a sane window so one lost packet cannot stretch
    /// the timeline.
    pub fn write_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        let duration = match self.prev_ts {
            None => DEFAULT_FRAME_TICKS,
            Some(prev) => {
                let delta = frame.rtp_timestamp.wrapping_sub(prev);
                if delta == 0 || delta > MAX_FRAME_TICKS {
                    self.last_duration
                } else {
                    delta
                }
            }
        };
        let sample = Mp4Sample {
            start_time: self.next_start,
            duration,
            rendering_offset: 0,
            is_sync: frame.is_keyframe,
            bytes: frame.data.clone(),
        };
        self.writer.write_sample(1, &sample)?;
        self.prev_ts = Some(frame.rtp_timestamp);
        self.last_duration = duration;
        self.next_start += duration as u64;
        self.frame_count += 1;
        Ok(())
    }

    /// Finalize: write `moov`, flush, and report the artifact.
    pub async fn finalize(mut self) -> Result<Mp4Artifact> {
        self.writer.write_end()?;
        self.writer.into_writer().flush()?;
        let file_size = tokio::fs::metadata(&self.path)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        // Wall-clock duration is the honest figure when packets were dropped
        // (the track timeline compresses gaps); fall back to the track sum.
        let wall_secs = if self.started_wall_micros > 0 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(self.started_wall_micros);
            (now.saturating_sub(self.started_wall_micros)) as f64 / 1_000_000.0
        } else {
            0.0
        };
        let track_secs = self.next_start as f64 / VIDEO_CLOCK_RATE as f64;
        let duration_secs = if wall_secs > 0.0 && wall_secs < track_secs {
            wall_secs
        } else {
            track_secs
        };
        Ok(Mp4Artifact {
            path: self.path.display().to_string(),
            duration_secs,
            file_size,
            frame_count: self.frame_count,
        })
    }

    /// Drop without finalizing (recorder replaced mid-call): the file keeps
    /// the mdat without moov and stays unplayable; callers use this only on
    /// error paths where the artifact is abandoned anyway.
    pub fn into_abandoned(self) -> PathBuf {
        self.path
    }
}

/// Open helper shared by the recorder: creates the parent directory so the
/// lazy muxer `File::create` cannot fail on a missing date subdir.
pub async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_recorder::assembler::H264FrameAssembler;

    fn nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x61 | nal_type.min(31)];
        bytes.extend_from_slice(body);
        bytes
    }

    /// Baseline 640x480 SPS body (bit-exact via the parser test builder is
    /// overkill here; a hand-fixed valid SPS is enough for muxing).
    fn test_sps() -> Vec<u8> {
        crate::video_recorder::sps::fixtures::baseline_sps(66, 40, 30, false)
    }

    fn test_pps() -> Vec<u8> {
        // PPS: pic_parameter_set_id=0, seq_parameter_set_id=0, trailing bits.
        vec![0x68, 0xCE, 0x3C, 0x80]
    }

    fn frame_at(assembler: &mut H264FrameAssembler, ts: u32, keyframe: bool) -> VideoFrame {
        let body: Vec<u8> = vec![0xAB; 64];
        let nal_bytes = nal(if keyframe { 5 } else { 1 }, &body);
        assembler
            .push_nal(&nal_bytes, ts, true)
            .expect("single-NAL frame")
    }

    #[tokio::test]
    async fn writes_valid_mp4_with_frames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("leg.mp4");
        let sps_nal = test_sps();
        let mut muxer =
            Mp4Muxer::create(&path, &sps_nal, &test_pps(), 0).expect("muxer created");

        let mut asm = H264FrameAssembler::new();
        // IDR + two inter frames at 30 fps.
        muxer.write_frame(&frame_at(&mut asm, 3000, true)).expect("frame 1");
        muxer.write_frame(&frame_at(&mut asm, 6000, false)).expect("frame 2");
        muxer.write_frame(&frame_at(&mut asm, 9000, false)).expect("frame 3");
        let artifact = muxer.finalize().await.expect("finalize");

        assert_eq!(artifact.frame_count, 3);
        assert!(artifact.file_size > 0, "file has bytes");
        let meta = tokio::fs::metadata(&artifact.path).await.expect("file exists");
        assert_eq!(meta.len(), artifact.file_size);

        // The mp4 crate's reader must accept the file back: 3 video samples,
        // first one marked as sync.
        let file = std::fs::File::open(&artifact.path).expect("open for read");
        let mut readable = mp4::read_mp4(file).expect("reader parses");
        let (_, track) = readable.tracks().iter().next().expect("video track");
        assert_eq!(track.track_type().expect("track type"), TrackType::Video);
        assert_eq!(readable.sample_count(1).expect("count"), 3);
        let first = readable
            .read_sample(1, 1)
            .expect("sample 0")
            .expect("present");
        assert!(first.is_sync);
    }

    #[tokio::test]
    async fn timestamp_gap_clamps_duration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("leg-gap.mp4");
        let mut muxer = Mp4Muxer::create(&path, &test_sps(), &test_pps(), 0).expect("muxer");
        let mut asm = H264FrameAssembler::new();
        muxer.write_frame(&frame_at(&mut asm, 3000, true)).expect("f1");
        // Huge gap (stream discontinuity): duration clamps, timeline stays
        // consistent for players.
        muxer.write_frame(&frame_at(&mut asm, 3000 + 10 * VIDEO_CLOCK_RATE, false))
            .expect("f2");
        let artifact = muxer.finalize().await.expect("finalize");
        assert_eq!(artifact.frame_count, 2);
    }
}
