use super::VadEngine;
use crate::{AudioFrame, Samples};
use anyhow::Result;
use webrtc_vad::{SampleRate, Vad, VadMode};

pub struct WebRtcVad {
    vad: Vad,
}

impl WebRtcVad {
    pub fn new(samplerate: u32) -> Result<Self> {
        let sample_rate = match samplerate {
            8000 => SampleRate::Rate8kHz,
            16000 => SampleRate::Rate16kHz,
            _ => return Err(anyhow::anyhow!("Unsupported sample rate: {}", samplerate)),
        };

        Ok(Self {
            vad: Vad::new_with_rate_and_mode(sample_rate, VadMode::VeryAggressive),
        })
    }
}
unsafe impl Send for WebRtcVad {}
unsafe impl Sync for WebRtcVad {}

impl VadEngine for WebRtcVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Option<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Some((false, frame.timestamp)),
        };

        Some((
            self.vad.is_voice_segment(samples).unwrap_or(false),
            frame.timestamp,
        ))
    }

    fn get_last_score(&self) -> Option<f32> {
        None // WebRTC VAD doesn't provide probability scores
    }
}
