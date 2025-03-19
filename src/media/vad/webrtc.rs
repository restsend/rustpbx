use super::VADConfig;
use crate::media::{processor::Processor, stream::EventSender};

pub struct WebRTCVadProcessor {
    event_sender: EventSender,
    config: VADConfig,
}

impl WebRTCVadProcessor {
    pub fn new(event_sender: EventSender, config: VADConfig) -> Self {
        Self {
            event_sender,
            config,
        }
    }
}

impl Processor for WebRTCVadProcessor {
    fn process<'a>(&self, ts: u32, pcm: &'a Vec<f32>) -> Option<&'a Vec<f32>> {
        return Some(pcm);
    }
}
