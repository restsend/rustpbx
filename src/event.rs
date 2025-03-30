use crate::AudioFrame;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    #[serde(rename = "audio")]
    Audio(AudioFrame),
    #[serde(rename = "start_speaking")]
    StartSpeaking(String, u32),
    #[serde(rename = "silence")]
    Silence(String, u32),
    #[serde(rename = "dtmf")]
    DTMF(String, u32, String),
    #[serde(rename = "track_start")]
    TrackStart(String, u32),
    #[serde(rename = "track_end")]
    TrackEnd(String, u32),
    /// track_id, timestamp, text
    #[serde(rename = "transcription_final")]
    TranscriptionFinal(String, u32, String),
    /// track_id, timestamp, text
    #[serde(rename = "transcription_delta")]
    TranscriptionDelta(String, u32, String),
    /// timestamp, text
    #[serde(rename = "llm_final")]
    LLMFinal(u32, String),
    /// track_id, timestamp,  word
    #[serde(rename = "llm_delta")]
    LLMDelta(u32, String),
    /// timestamp, audio
    #[serde(rename = "synthesis")]
    Synthesis(u32, Vec<u8>),
    /// timestamp, metrics
    #[serde(rename = "metrics")]
    Metrics(u32, serde_json::Value),
    /// timestamp, error message
    #[serde(rename = "error")]
    Error(u32, String),
}

impl SessionEvent {
    pub fn timestamp(&self) -> u32 {
        match self {
            SessionEvent::Audio(frame) => frame.timestamp,
            SessionEvent::DTMF(_, timestamp, _) => *timestamp,
            SessionEvent::TrackStart(_, timestamp) => *timestamp,
            SessionEvent::TrackEnd(_, timestamp) => *timestamp,
            SessionEvent::TranscriptionFinal(_, timestamp, _) => *timestamp,
            SessionEvent::TranscriptionDelta(_, timestamp, _) => *timestamp,
            SessionEvent::LLMDelta(timestamp, _) => *timestamp,
            SessionEvent::LLMFinal(timestamp, _) => *timestamp,
            SessionEvent::Metrics(timestamp, _) => *timestamp,
            SessionEvent::Error(timestamp, _) => *timestamp,
            SessionEvent::StartSpeaking(_, timestamp) => *timestamp,
            SessionEvent::Silence(_, timestamp) => *timestamp,
            SessionEvent::Synthesis(timestamp, _) => *timestamp,
        }
    }
}

pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;
