use super::track::TrackId;
use anyhow::Result;

#[derive(Clone)]
pub enum AudioPayload {
    PCM(Vec<i16>),
    RTP(u8, Vec<u8>),
    Empty,
}
#[derive(Clone)]
pub struct AudioFrame {
    pub track_id: TrackId,
    pub samples: AudioPayload,
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
            samples: AudioPayload::Empty,
            timestamp: 0,
            sample_rate: 16000,
        }
    }
}

impl AudioPayload {
    pub fn is_empty(&self) -> bool {
        match self {
            AudioPayload::PCM(samples) => samples.is_empty(),
            AudioPayload::RTP(_, payload) => payload.is_empty(),
            AudioPayload::Empty => true,
        }
    }
}
