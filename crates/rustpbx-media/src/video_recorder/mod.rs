//! Video recording: H264 RTP → MP4 per-leg artifacts.
//!
//! Layers (all synchronous, owned by the recording task):
//! - [`assembler`]: rustrtc `H264Depacketizer` NALs → AVCC access units
//!   (SPS/PPS captured for `avcC`, keyframe flag per frame).
//! - [`sps`]: minimal SPS parser for picture dimensions.
//! - [`muxer`]: `mp4` crate writer, 90 kHz video track, `moov` on finalize.
//! - [`recorder`]: [`Mp4Recorder`] (per-`CaptureChannel` files, IDR-aligned
//!   start) and [`CompositeMediaRecorder`] (WAV + MP4 in one task).
//!
//! Audio deliberately stays in the WAV artifact: no pure-Rust MP4 muxer
//! supports Opus and no pure-Rust AAC encoder exists, so adding a C audio
//! encoder is a product decision (see docs/config recording video section).

pub mod assembler;
pub mod muxer;
pub mod recorder;
pub mod sps;

pub use assembler::{H264FrameAssembler, VideoFrame};
pub use muxer::{Mp4Artifact, Mp4Muxer, VIDEO_CLOCK_RATE};
pub use recorder::{CompositeMediaRecorder, Mp4Recorder, Mp4RecorderOptions};
pub use sps::SpsGeometry;
