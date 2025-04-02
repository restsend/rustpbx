use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    #[serde(rename = "answer")]
    Answer {
        track_id: String,
        timestamp: u64,
        sdp: String,
    },
    #[serde(rename = "reject")]
    Reject {
        track_id: String,
        timestamp: u64,
        reason: String,
    },
    #[serde(rename = "ringing")]
    Ringing {
        track_id: String,
        timestamp: u64,
        early_media: bool,
    },
    #[serde(rename = "start_speaking")]
    StartSpeaking { track_id: String, timestamp: u64 },
    #[serde(rename = "silence")]
    Silence { track_id: String, timestamp: u64 },
    #[serde(rename = "dtmf")]
    DTMF {
        track_id: String,
        timestamp: u64,
        digit: String,
    },
    #[serde(rename = "track_start")]
    TrackStart { track_id: String, timestamp: u64 },
    #[serde(rename = "track_end")]
    TrackEnd { track_id: String, timestamp: u64 },
    /// track_id, timestamp, text
    #[serde(rename = "transcription_final")]
    TranscriptionFinal {
        track_id: String,
        timestamp: u64,
        index: u32,
        start_time: Option<u32>,
        end_time: Option<u32>,
        text: String,
    },
    /// track_id, timestamp, text
    #[serde(rename = "transcription_delta")]
    TranscriptionDelta {
        track_id: String,
        index: u32,
        timestamp: u64,
        start_time: Option<u32>,
        end_time: Option<u32>,
        text: String,
    },
    /// timestamp, text
    #[serde(rename = "llm_final")]
    LLMFinal { timestamp: u64, text: String },
    /// track_id, timestamp,  word
    #[serde(rename = "llm_delta")]
    LLMDelta { timestamp: u64, word: String },
    /// timestamp, audio
    #[serde(rename = "synthesis")]
    Synthesis { timestamp: u64, audio: Vec<u8> },
    /// timestamp, metrics
    #[serde(rename = "metrics")]
    Metrics {
        timestamp: u64,
        sender: String,
        metrics: serde_json::Value,
    },
    /// timestamp, error message
    #[serde(rename = "error")]
    Error { timestamp: u64, error: String },
}

impl SessionEvent {
    pub fn timestamp(&self) -> u64 {
        match self {
            SessionEvent::Answer { timestamp, .. } => *timestamp,
            SessionEvent::Reject { timestamp, .. } => *timestamp,
            SessionEvent::Ringing { timestamp, .. } => *timestamp,
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

pub fn create_event_sender() -> EventSender {
    EventSender::new(128)
}

// Helper function for testing
#[cfg(test)]
pub mod test_utils {
    use super::*;

    pub fn dummy_event_sender() -> EventSender {
        create_event_sender()
    }
}
