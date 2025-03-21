use anyhow::Result;
use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use crate::media::processor::AudioFrame;

pub struct RecorderConfig {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            channels: 1,
        }
    }
}

pub struct Recorder {
    track_id: String,
    config: RecorderConfig,
    file: Option<File>,
    track_buffers: Arc<Mutex<HashMap<String, Vec<f32>>>>,
    samples_written: u32,
}

impl Recorder {
    pub fn new(track_id: String, config: RecorderConfig) -> Self {
        Self {
            track_id,
            config,
            file: None,
            track_buffers: Arc::new(Mutex::new(HashMap::new())),
            samples_written: 0,
        }
    }

    pub async fn process_frame(&mut self, frame: AudioFrame) -> Result<()> {
        // Convert i16 samples to f32 for easier mixing
        let f32_samples: Vec<f32> = frame
            .samples
            .iter()
            .map(|&sample| sample as f32 / 32767.0)
            .collect();

        // Store the samples in the track buffer
        let mut track_buffers = self.track_buffers.lock().unwrap();
        track_buffers.insert(frame.track_id.clone(), f32_samples);

        // Mix all track buffers
        let mixed_samples = self.mix_audio(&track_buffers);

        // Write mixed samples to file if needed
        if let Some(file) = &mut self.file {
            // Convert back to i16 for storage
            let i16_samples: Vec<i16> = mixed_samples
                .iter()
                .map(|&sample| (sample * 32767.0) as i16)
                .collect();

            // Write as raw PCM
            let bytes: Vec<u8> = i16_samples
                .iter()
                .flat_map(|&sample| sample.to_le_bytes().to_vec())
                .collect();

            file.write_all(&bytes)?;
            self.samples_written += (bytes.len() / 2) as u32;
        }

        Ok(())
    }

    // Mix audio from multiple tracks
    fn mix_audio(&self, track_buffers: &HashMap<String, Vec<f32>>) -> Vec<f32> {
        // Find the length of the longest buffer
        let max_len = track_buffers
            .values()
            .map(|buf| buf.len())
            .max()
            .unwrap_or(0);

        if max_len == 0 {
            return vec![];
        }

        // Create output buffer filled with zeros
        let mut mixed = vec![0.0; max_len];

        // If we have multiple tracks, we need to mix them
        if track_buffers.len() > 1 {
            // Count of active tracks for normalization
            let track_count = track_buffers.len() as f32;

            // Mix all tracks
            for samples in track_buffers.values() {
                for (i, &sample) in samples.iter().enumerate() {
                    if i < mixed.len() {
                        mixed[i] += sample / track_count; // Simple averaging mix
                    }
                }
            }

            // Optional: Apply some dynamic range compression to avoid clipping
            for sample in &mut mixed {
                // Soft clipping to avoid harsh distortion
                if *sample > 0.95 {
                    *sample = 0.95 + (*sample - 0.95) * 0.05;
                } else if *sample < -0.95 {
                    *sample = -0.95 + (*sample + 0.95) * 0.05;
                }
            }
        } else if let Some(samples) = track_buffers.values().next() {
            // If only one track, just copy it
            for (i, &sample) in samples.iter().enumerate() {
                if i < mixed.len() {
                    mixed[i] = sample;
                }
            }
        }

        mixed
    }

    // Method to start recording to a file
    pub fn start_recording(&mut self, file_path: &Path) -> Result<()> {
        if self.file.is_none() {
            self.file = Some(File::create(file_path)?);
            self.samples_written = 0;
        }
        Ok(())
    }

    // Method to stop recording
    pub fn stop_recording(&mut self) -> Result<()> {
        self.file = None;
        Ok(())
    }
}
