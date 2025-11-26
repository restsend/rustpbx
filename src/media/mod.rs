use serde::{Deserialize, Serialize};

pub mod asr_processor;
pub mod cache;
pub mod codecs;
pub mod denoiser;
pub mod dtmf;
pub mod engine;
pub mod jitter;
pub mod negotiate;
pub mod processor;
pub mod recorder;
pub mod stream;
#[cfg(test)]
mod tests;
pub mod track;
pub mod vad;
pub mod volume_control;

pub type TrackId = String;
pub type Sample = i16;
pub type PcmBuf = Vec<Sample>;
pub type PayloadBuf = Vec<u8>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Samples {
    PCM {
        samples: PcmBuf,
    },
    RTP {
        sequence_number: u16,
        payload_type: u8,
        payload: PayloadBuf,
    },
    Empty,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFrame {
    pub track_id: TrackId,
    pub samples: Samples,
    pub timestamp: u64,
    pub sample_rate: u32,
}

impl Samples {
    pub fn payload_type(&self) -> Option<u8> {
        match self {
            Samples::RTP { payload_type, .. } => Some(*payload_type),
            _ => None,
        }
    }
}
// get timestamp in milliseconds
pub fn get_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64
}
