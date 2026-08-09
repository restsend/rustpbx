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
pub mod media_bridge;
pub mod media_recorder;
pub mod media_stream;
pub mod mixer;
pub mod negotiate;
pub mod recorder;
pub mod rtc_track;
pub mod rtp_track_builder;
pub mod telephone_event;
pub mod track;
pub mod wav_reader;
pub mod wav_writer;

#[cfg(test)]
mod media_track_tests;
#[cfg(test)]
mod mixer_e2e_tests;
#[cfg(test)]
mod recorder_tests;

// ── Re-exports ──────────────────────────────────────────────────────
pub use audio_codec::CodecType;
pub use conference_mixer::ConferenceAudioMixer;

pub use leg_id::LegId;
pub use media_stream::TrackMap;
pub use media_stream::{MediaStream, MediaStreamBuilder};
pub use mixer::AudioMixer;
pub use negotiate::{CodecInfo, MediaNegotiator};
pub use rtc_track::RtcTrack;
pub use rtp_track_builder::RtpTrackBuilder;
pub use track::Track;

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


