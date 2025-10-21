use serde::{Deserialize, Serialize};

pub mod app;
pub mod call;
pub mod callrecord;
pub mod config;
pub mod event;
pub mod handler;
pub mod llm;
pub mod media;
pub mod models;
pub mod net_tool;
pub mod preflight;
pub mod proxy;
pub mod synthesis;
pub mod transcription;
pub mod useragent;
pub mod version;

pub type TrackId = String;
pub type Sample = i16;
pub type PcmBuf = Vec<Sample>;
pub type PayloadBuf = Vec<u8>;
#[cfg(feature = "console")]
pub mod console; // Admin console

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
