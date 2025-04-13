use super::{VADConfig, VadEngine};
use crate::{AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use voice_activity_detector::VoiceActivityDetector;

pub struct SileroVad {
    detector: VoiceActivityDetector,
    config: VADConfig,
    buffer: PcmBuf,
    last_timestamp: u64,
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
            last_timestamp: 0,
        })
    }
}

impl VadEngine for SileroVad {
    fn process(&mut self, frame: &mut AudioFrame) -> Option<(bool, u64)> {
        let samples = match &frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Some((false, frame.timestamp)),
        };
        if self.buffer.len() < self.chunk_size {
            self.buffer.extend_from_slice(samples);
            if self.last_timestamp == 0 {
                self.last_timestamp = frame.timestamp;
            } else {
                self.last_timestamp = (self.last_timestamp + frame.timestamp) / 2;
            }
        }
        if self.buffer.len() >= self.chunk_size {
            let chunk = self.buffer[..self.chunk_size].to_vec();
            let score = self.detector.predict(chunk);
            let is_voice = score > self.config.voice_threshold;
            self.buffer.drain(..self.chunk_size);
            return Some((is_voice, self.last_timestamp));
        }
        None
    }
}
