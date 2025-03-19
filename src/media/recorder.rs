use anyhow::Result;
use std::{
    collections::HashMap,
    fs::File,
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
        todo!()
    }
}
