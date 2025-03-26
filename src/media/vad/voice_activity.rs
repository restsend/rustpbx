use anyhow::Result;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use webrtc_vad::{SampleRate, Vad, VadMode};

use super::{ThreadSafeVad, VADConfig};
use crate::media::processor::{AudioFrame, AudioPayload};

pub struct VoiceActivityVad {
    detector: ThreadSafeVad,
    config: VADConfig,
    buffer: Vec<i16>,
    last_speech_time: Option<Instant>,
    is_speaking: bool,
    speech_start_time: Option<u64>,
}

impl VoiceActivityVad {
    pub fn new() -> Self {
        Self {
            detector: ThreadSafeVad(Mutex::new(Vad::new_with_rate_and_mode(
                SampleRate::Rate16kHz,
                VadMode::Quality,
            ))),
            config: VADConfig::default(),
            buffer: Vec::new(),
            last_speech_time: None,
            is_speaking: false,
            speech_start_time: None,
        }
    }
}

impl super::VadEngine for VoiceActivityVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Result<bool> {
        // Add current frame to buffer
        let samples = match &frame.samples {
            AudioPayload::PCM(samples) => samples,
            _ => return Ok(false),
        };
        self.buffer.extend_from_slice(samples);

        // Process in chunks of 30ms (480 samples at 16kHz)
        let chunk_size = (16000 * 30) / 1000;
        let mut is_speaking = self.is_speaking;

        while self.buffer.len() >= chunk_size {
            let chunk = self.buffer[..chunk_size].to_vec();
            let is_voice = self
                .detector
                .0
                .lock()
                .unwrap()
                .is_voice_segment(&chunk)
                .unwrap_or(false);

            // Remove processed samples
            self.buffer.drain(..chunk_size);

            // Calculate timestamp in milliseconds
            let timestamp = (self.buffer.len() as u64 * 1000) / frame.sample_rate as u64;

            if is_voice {
                self.last_speech_time = Some(Instant::now());
                if !self.is_speaking {
                    self.is_speaking = true;
                    self.speech_start_time = Some(timestamp);
                    is_speaking = true;
                }
            } else if self.is_speaking {
                if let Some(last_time) = self.last_speech_time {
                    let silence_duration = Instant::now().duration_since(last_time);
                    if silence_duration
                        > Duration::from_millis(self.config.silence_duration_threshold)
                    {
                        self.is_speaking = false;
                        self.speech_start_time = None;
                        is_speaking = false;
                    }
                }
            }
        }

        Ok(is_speaking)
    }
}
