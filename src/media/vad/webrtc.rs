use anyhow::Result;
use webrtc_vad::{SampleRate, Vad, VadMode};

use super::{ThreadSafeVad, VadEngine};
use crate::media::{processor::AudioFrame, utils};

pub struct WebRtcVad {
    vad: ThreadSafeVad,
}

impl WebRtcVad {
    pub fn new() -> Self {
        Self {
            vad: ThreadSafeVad(Vad::new_with_rate_and_mode(
                SampleRate::Rate16kHz,
                VadMode::Quality,
            )),
        }
    }
}

impl VadEngine for WebRtcVad {
    fn process(&mut self, frame: &AudioFrame) -> Result<bool> {
        // Convert f32 samples to i16 for WebRTC VAD
        let pcm = utils::convert_f32_to_pcm(&frame.samples);

        // WebRTC VAD expects 10, 20, or 30ms frames
        // For 16kHz, that's 160, 320, or 480 samples
        let frame_size = match pcm.len() {
            160 | 320 | 480 => pcm.len(),
            _ => return Ok(false), // Invalid frame size
        };

        Ok(self
            .vad
            .0
            .is_voice_segment(&pcm[..frame_size])
            .unwrap_or(false))
    }
}
