//! Media Engine — protocol-agnostic media processing.
//!
//! This crate provides all real-time media operations (bridging, playback,
//! recording, DTMF, MCU mixing, transcoding) with zero SIP dependencies.

pub mod app_ingress;
pub mod audio_source;
pub mod conference_mixer;
pub mod dtmf;
pub mod egress;
pub mod ingress_tap;
pub mod leg;
pub mod leg_id;
pub mod leg_stats;
pub mod media_bridge;
pub mod media_recorder;
pub mod media_stream;
pub mod mixer;
pub mod negotiate;
pub mod recorder;
pub mod rtc_track;
pub mod rtp_track_builder;
pub mod telephone_event;
pub mod telemetry;
pub mod track;
pub mod wav_reader;
pub mod wav_writer;

#[cfg(test)]
mod media_track_tests;
#[cfg(test)]
mod mixer_tests;
#[cfg(test)]
mod recorder_tests;
// ── Re-exports ──────────────────────────────────────────────────────
pub use audio_codec::CodecType;
pub use conference_mixer::ConferenceAudioMixer;

pub use leg_id::LegId;
pub use media_stream::{MediaStream, MediaStreamBuilder};
pub use mixer::AudioMixer;
pub use negotiate::{CodecInfo, MediaNegotiator};
pub use rtc_track::RtcTrack;
pub use rtp_track_builder::RtpTrackBuilder;
pub use track::Track;

// ── Shared utility types ────────────────────────────────────────────

/// Audio frame buffer for passing PCM audio between components.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Raw PCM samples (16-bit signed, mono)
    pub samples: Vec<i16>,
    /// Sample rate
    pub sample_rate: u32,
    /// Timestamp
    pub timestamp: u64,
}

impl AudioFrame {
    /// Create a new audio frame.
    pub fn new(samples: Vec<i16>, sample_rate: u32) -> Self {
        Self {
            samples,
            sample_rate,
            timestamp: 0,
        }
    }
}

pub fn get_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
