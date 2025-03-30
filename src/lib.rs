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
pub mod useragent;
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
    pub sample_rate: u16,
}
