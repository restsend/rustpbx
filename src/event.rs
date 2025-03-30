use crate::AudioFrame;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    #[serde(rename = "audio")]
    Audio { frame: AudioFrame },
    #[serde(rename = "start_speaking")]
    StartSpeaking { track_id: String, timestamp: u32 },
    #[serde(rename = "silence")]
    Silence { track_id: String, timestamp: u32 },
    #[serde(rename = "dtmf")]
    DTMF {
        track_id: String,
        timestamp: u32,
        digit: String,
    },
    #[serde(rename = "track_start")]
    TrackStart { track_id: String, timestamp: u32 },
    #[serde(rename = "track_end")]
    TrackEnd { track_id: String, timestamp: u32 },
    /// track_id, timestamp, text
    #[serde(rename = "transcription_final")]
    TranscriptionFinal {
        track_id: String,
        timestamp: u32,
        text: String,
    },
    /// track_id, timestamp, text
    #[serde(rename = "transcription_delta")]
    TranscriptionDelta {
        track_id: String,
        timestamp: u32,
        text: String,
    },
    /// timestamp, text
    #[serde(rename = "llm_final")]
    LLMFinal { timestamp: u32, text: String },
    /// track_id, timestamp,  word
    #[serde(rename = "llm_delta")]
    LLMDelta { timestamp: u32, word: String },
    /// timestamp, audio
    #[serde(rename = "synthesis")]
    Synthesis { timestamp: u32, audio: Vec<u8> },
    /// timestamp, metrics
    #[serde(rename = "metrics")]
    Metrics {
        timestamp: u32,
        metrics: serde_json::Value,
    },
    /// timestamp, error message
    #[serde(rename = "error")]
    Error { timestamp: u32, error: String },
}

impl SessionEvent {
    pub fn timestamp(&self) -> u32 {
        match self {
            SessionEvent::Audio { frame } => frame.timestamp,
            SessionEvent::DTMF { timestamp, .. } => *timestamp,
            SessionEvent::TrackStart { timestamp, .. } => *timestamp,
            SessionEvent::TrackEnd { timestamp, .. } => *timestamp,
            SessionEvent::TranscriptionFinal { timestamp, .. } => *timestamp,
            SessionEvent::TranscriptionDelta { timestamp, .. } => *timestamp,
            SessionEvent::LLMDelta { timestamp, .. } => *timestamp,
            SessionEvent::LLMFinal { timestamp, .. } => *timestamp,
            SessionEvent::Metrics { timestamp, .. } => *timestamp,
            SessionEvent::Error { timestamp, .. } => *timestamp,
            SessionEvent::StartSpeaking { timestamp, .. } => *timestamp,
            SessionEvent::Silence { timestamp, .. } => *timestamp,
            SessionEvent::Synthesis { timestamp, .. } => *timestamp,
        }
    }
}

pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;
