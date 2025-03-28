use serde::{Deserialize, Serialize};

/// SessionEvent represents different types of events that can occur during a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    /// Audio is received
    #[serde(rename = "audio")]
    Audio(String, u32, Vec<i16>), // track_id, timestamp, samples

    /// DTMF is detected
    #[serde(rename = "dtmf")]
    DTMF(String, u32, String), // track_id, timestamp, digit

    /// Track started
    #[serde(rename = "track_start")]
    TrackStart(String), // track_id

    /// Track ended
    #[serde(rename = "track_end")]
    TrackEnd(String), // track_id

    /// Transcription from ASR
    #[serde(rename = "transcription")]
    Transcription(String, u32, String), // track_id, timestamp, text

    /// Transcription segment from ASR
    #[serde(rename = "transcription_segment")]
    TranscriptionSegment(String, u32, String), // track_id, timestamp, text

    /// LLM response
    #[serde(rename = "llm")]
    LLM(u32, String), // timestamp, text

    /// TTS/Synthesis event
    #[serde(rename = "tts")]
    TTS(u32, String), // timestamp, text

    /// Metrics event (for diagnostics)
    #[serde(rename = "metrics")]
    Metrics(u32, serde_json::Value), // timestamp, metrics

    /// Error event
    #[serde(rename = "error")]
    Error(u32, String), // timestamp, error message
}

impl SessionEvent {
    pub fn timestamp(&self) -> u32 {
        match self {
            SessionEvent::Audio(_, timestamp, _) => *timestamp,
            SessionEvent::DTMF(_, timestamp, _) => *timestamp,
            SessionEvent::TrackStart(_) => 0,
            SessionEvent::TrackEnd(_) => 0,
            SessionEvent::Transcription(_, timestamp, _) => *timestamp,
            SessionEvent::TranscriptionSegment(_, timestamp, _) => *timestamp,
            SessionEvent::LLM(timestamp, _) => *timestamp,
            SessionEvent::TTS(timestamp, _) => *timestamp,
            SessionEvent::Metrics(timestamp, _) => *timestamp,
            SessionEvent::Error(timestamp, _) => *timestamp,
        }
    }
}

/// Type alias for the event sender
pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;

/// Type alias for the event receiver
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;
