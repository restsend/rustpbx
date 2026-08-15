//! Shared recording primitives.
//!
//! Both recording pipelines depend on this crate:
//! - real-time recording (`rustpbx-media`: recorder.rs, wav_writer.rs)
//! - stored-capture export (`rustpbx-sipflow`: wav_utils.rs)
//!
//! Only stateless primitives live here — DTMF synthesis/parsing, WAV header
//! knowledge, coded-domain silence/mix/interleave, the decode+resample
//! pipeline and the payload-type tables. Timeline *policies* (wall-clock
//! anchoring for export vs. sequential append for real-time) intentionally
//! stay in their consumers.

pub mod dtmf;
pub mod mix;
pub mod payload;
pub mod pipeline;
pub mod wav;

pub use dtmf::{
    DtmfGenerator, dtmf_char_to_code, dtmf_code_to_char, dtmf_duration_samples,
    looks_like_dtmf_payload, parse_dtmf_event,
};
pub use mix::{MixMode, interleave_blocks, mix_pcm, silence_chunk};
pub use payload::{PayloadDescriptor, codec_from_rtpmap_name, default_payload_descriptor};
pub use pipeline::LegCodecPipeline;
pub use wav::{FORMAT_G729, FORMAT_PCM, FORMAT_PCMA, FORMAT_PCMU, WavSpec, wav_header};
