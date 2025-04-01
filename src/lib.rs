use serde::{Deserialize, Serialize};

pub mod app;
pub mod config;
pub mod console;
pub mod error;
pub mod event;
pub mod handler;
pub mod llm;
pub mod media;
pub mod proxy;
pub mod synthesis;
pub mod transcription;
//pub mod useragent;
pub use error::Error;
pub type TrackId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Samples {
    PCM(Vec<i16>),
    RTP(u8, Vec<u8>),
    Empty,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFrame {
    pub track_id: TrackId,
    pub samples: Samples,
    pub timestamp: u32,
    pub sample_rate: u32,
}
// get timestamp in milliseconds
pub fn get_timestamp() -> u64 {
    let now = std::time::SystemTime::now();
    now.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
