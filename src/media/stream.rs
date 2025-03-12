use std::{collections::HashMap, sync::Mutex};

use super::{
    recorder::Recorder,
    track::{Track, TrackDirection},
    vad::VADConfig,
};
use anyhow::Result;
use tracing::{error, info};

pub struct MediaStreamConfig {
    recorder: Option<String>, // Path to save the PCM recording
    vad: Option<VADConfig>,   // VAD (Voice Activity Detection)
}

#[derive(Debug, Clone)]
pub enum MediaStreamEvent {
    DTMF(TrackDirection, u32, String),
    StartSpeaking(TrackDirection, u32),
    Silence(TrackDirection, u32),
    Transcription(TrackDirection, u32, String), // word-level transcription
    TranscriptionSegment(TrackDirection, u32, String), // segment-level transcription
    TrackStart(TrackDirection),
    TrackStop(TrackDirection),
}

pub struct MediaStream {
    pub config: MediaStreamConfig,
    pub inbound: Option<Box<dyn Track>>,
    pub outbound: Mutex<Option<Box<dyn Track>>>,
    pub recorder: Option<Recorder>,
}

impl MediaStream {
    pub fn new(config: MediaStreamConfig) -> Self {
        Self {
            config,
            inbound: None,
            outbound: Mutex::new(None),
            recorder: None,
        }
    }

    pub async fn process(&mut self) -> Result<MediaStreamEvent> {
        todo!()
    }

    pub fn remove_track(&mut self, direction: TrackDirection) {
        todo!()
    }

    pub fn update_track(&mut self, direction: TrackDirection, track: Box<dyn Track>) {
        todo!()
    }
}
