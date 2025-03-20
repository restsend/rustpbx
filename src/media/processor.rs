use super::track::TrackId;
use anyhow::Result;
#[derive(Clone)]
pub struct AudioFrame {
    pub track_id: TrackId,
    pub samples: Vec<i16>,
    pub timestamp: u32,
    pub sample_rate: u16,
}
pub trait Processor: Send + Sync {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()>;
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self {
            track_id: "".to_string(),
            samples: vec![],
            timestamp: 0,
            sample_rate: 16000,
        }
    }
}
