use super::track::TrackId;
use anyhow::Result;
#[derive(Clone)]
pub struct AudioFrame {
    pub track_id: TrackId,
    pub samples: Vec<f32>,
    pub timestamp: u32,
    pub sample_rate: u32,
    pub channels: u16,
}
pub trait Processor: Send + Sync {
    fn process_frame(&mut self, frame: AudioFrame) -> Result<AudioFrame>;
}
