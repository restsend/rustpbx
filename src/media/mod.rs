//! Media Engine — protocol-agnostic media processing.
//!
//! This crate provides all real-time media operations (bridging, playback,
//! recording, DTMF, MCU mixing, transcoding) with zero SIP dependencies.
//! Inputs and outputs are expressed as SDP strings, [`MediaCommand`]s, and
//! [`MediaEvent`]s.

pub mod audio_source;
pub mod bridge;
pub mod capture;
pub mod engine;
pub mod forwarding_track;
pub mod leg_id;
pub mod media_stream;
pub mod mixer;
pub mod negotiate;
pub mod recorder;
pub mod rtc_track;
pub mod rtp_track_builder;
pub mod telephone_event;
pub mod track;
pub mod file_track;
pub mod transcoder;
pub mod transcoding_pipeline;
pub mod wav_reader;
pub mod wav_writer;
pub mod conference_mixer;
pub mod dtmf;

#[cfg(test)]
mod file_track_tests;
#[cfg(test)]
mod mixer_e2e_tests;
#[cfg(test)]
mod media_track_tests;
#[cfg(test)]
mod media_engine_tests;
#[cfg(test)]
mod recorder_tests;
#[cfg(test)]
mod unified_pc_tests;

// ── Re-exports ──────────────────────────────────────────────────────
pub use audio_codec::CodecType;
pub use conference_mixer::ConferenceAudioMixer;


pub(crate) use file_track::FileTrackPlaybackSource;
pub use file_track::{FileTrack, PlaybackEndReason, PlaybackEndCallback};
pub use leg_id::LegId;
pub use media_stream::{MediaStream, MediaStreamBuilder};
pub use mixer::AudioMixer;
pub use negotiate::{CodecInfo, MediaNegotiator};
pub use rtc_track::RtcTrack;
pub use rtp_track_builder::RtpTrackBuilder;
pub use track::Track;
pub use media_stream::TrackMap;
pub use transcoder::Transcoder;// Tests in `file_track_tests` need this via `use super::*`.
#[cfg(test)]
pub(crate) use file_track::audio_frame_timing;

// ── Shared utility types ────────────────────────────────────────────

use anyhow::Result;

pub trait StreamWriter: Send + Sync {
    fn write_header(&mut self) -> Result<()>;
    fn write_packet(&mut self, data: &[u8], samples: usize) -> Result<()>;
    fn finalize(&mut self) -> Result<()>;
}

pub fn get_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone)]
pub struct ReceiveTimestampClock {
    base_instant: std::time::Instant,
    base_epoch_micros: u64,
}

impl ReceiveTimestampClock {
    pub fn new() -> Self {
        let base_epoch_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_micros() as u64)
            .unwrap_or_default();

        Self {
            base_instant: std::time::Instant::now(),
            base_epoch_micros,
        }
    }

    pub fn now_micros(&self) -> u64 {
        self.base_epoch_micros
            .saturating_add(self.base_instant.elapsed().as_micros() as u64)
    }
}

impl Default for ReceiveTimestampClock {
    fn default() -> Self {
        Self::new()
    }
}
