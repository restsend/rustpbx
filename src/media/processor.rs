use crate::{AudioFrame, Samples};
use anyhow::Result;

pub trait Processor: Send + Sync {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()>;
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self {
            track_id: "".to_string(),
            samples: Samples::Empty,
            timestamp: 0,
            sample_rate: 16000,
        }
    }
}

impl Samples {
    pub fn is_empty(&self) -> bool {
        match self {
            Samples::PCM(samples) => samples.is_empty(),
            Samples::RTP(_, payload) => payload.is_empty(),
            Samples::Empty => true,
        }
    }
}
