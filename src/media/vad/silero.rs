use super::{VADConfig, VadEngine};
use crate::{AudioFrame, Samples};
use anyhow::Result;
use voice_activity_detector::VoiceActivityDetector;

pub struct SileroVad {
    detector: VoiceActivityDetector,
    config: VADConfig,
    buffer: Vec<i16>,
    chunk_size: usize,
}

impl SileroVad {
    pub fn new(samplerate: u32) -> Result<Self> {
        // Create detector with default settings for 16kHz audio
        // Using 512 samples chunk size (32ms at 16kHz) - required by voice_activity_detector
        let chunk_size = match samplerate {
            8000 => 256,
            16000 => 512,
            32000 => 1024,
            48000 => 1536,
            _ => return Err(anyhow::anyhow!("Unsupported sample rate: {}", samplerate)),
        };
        let detector = VoiceActivityDetector::builder()
            .sample_rate(samplerate)
            .chunk_size(chunk_size)
            .build()
            .expect("Failed to build voice activity detector");

        // Use a custom config with a lower threshold specifically tuned for the voice_activity_detector
        let config = VADConfig::default();

        Ok(Self {
            detector,
            config,
            buffer: Vec::new(),
            chunk_size,
        })
    }
}

impl VadEngine for SileroVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Result<bool> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(false),
        };

        self.buffer.extend_from_slice(samples);

        while self.buffer.len() >= self.chunk_size {
            let chunk = self.buffer[..self.chunk_size].to_vec();
            let score = self.detector.predict(chunk);
            let is_voice = score > self.config.voice_threshold;
            self.buffer.drain(..self.chunk_size);
            if is_voice {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
