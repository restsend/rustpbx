use super::{ThreadSafeVad, VadEngine};
use crate::{AudioFrame, Samples};
use anyhow::Result;
use std::sync::Mutex;
use webrtc_vad::{SampleRate, Vad, VadMode};

pub struct WebRtcVad {
    vad: ThreadSafeVad,
}

impl WebRtcVad {
    pub fn new() -> Self {
        Self {
            vad: ThreadSafeVad(Mutex::new(Vad::new_with_rate_and_mode(
                SampleRate::Rate16kHz,
                VadMode::Quality,
            ))),
        }
    }
}

impl VadEngine for WebRtcVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Result<bool> {
        // Process in chunks of 30ms (480 samples at 16kHz)
        let samples = match &frame.samples {
            Samples::PCM(samples) => samples,
            _ => return Ok(false),
        };

        Ok(self
            .vad
            .0
            .lock()
            .unwrap()
            .is_voice_segment(samples)
            .unwrap_or(false))
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
