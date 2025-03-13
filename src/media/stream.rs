use super::{
    recorder::Recorder,
    track::{Track, TrackDirection},
    vad::VADConfig,
};
use anyhow::Result;
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};

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
    Shutdown,
}

pub type EventSender = broadcast::Sender<MediaStreamEvent>;
pub type EventReceiver = broadcast::Receiver<MediaStreamEvent>;

pub enum MediaStreamCommand {
    RemoveTrack(TrackDirection),
    UpdateTrack(TrackDirection, Box<dyn Track>),
}

pub struct MediaStream {
    pub config: MediaStreamConfig,
    pub inbound: Option<Box<dyn Track>>,
    pub outbound: Mutex<Option<Box<dyn Track>>>,
    pub cmd_tx: mpsc::UnboundedSender<MediaStreamCommand>,
    pub cmd_rx: mpsc::UnboundedReceiver<MediaStreamCommand>,
}

impl MediaStream {
    pub fn new(config: MediaStreamConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        Self {
            config,
            inbound: None,
            outbound: Mutex::new(None),
            cmd_tx,
            cmd_rx,
        }
    }

    pub async fn process(&mut self) -> Result<MediaStreamEvent> {
        todo!()
    }

    pub fn remove_track(&self, direction: TrackDirection) -> Result<()> {
        self.cmd_tx
            .send(MediaStreamCommand::RemoveTrack(direction))
            .map_err(Into::into)
    }

    pub fn update_track(&self, direction: TrackDirection, track: Box<dyn Track>) -> Result<()> {
        self.cmd_tx
            .send(MediaStreamCommand::UpdateTrack(direction, track))
            .map_err(Into::into)
    }
}
