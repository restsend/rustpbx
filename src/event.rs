use crate::PcmBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "camelCase")]
pub enum SessionEvent {
    Answer {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        sdp: String,
    },
    Reject {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        reason: String,
    },
    Ringing {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        #[serde(rename = "earlyMedia")]
        early_media: bool,
    },
    Hangup {
        timestamp: u64,
        reason: String,
    },
    AnswerMachineDetection {
        // Answer machine detection
        timestamp: u64,
        #[serde(rename = "startTime")]
        start_time: u64,
        #[serde(rename = "endTime")]
        end_time: u64,
        text: String,
    },
    Speaking {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        #[serde(rename = "startTime")]
        start_time: u64,
    },
    Silence {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        #[serde(rename = "startTime")]
        start_time: u64,
        duration: u64,
        #[serde(skip)]
        samples: Option<PcmBuf>,
    },
    Noisy {
        // Track is noisy, no need to process
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        #[serde(rename = "startTime")]
        start_time: u64,
    },
    DTMF {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        digit: String,
    },
    TrackStart {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
    },
    TrackEnd {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
    },
    AsrFinal {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        index: u32,
        #[serde(rename = "startTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        start_time: Option<u32>,
        #[serde(rename = "endTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        end_time: Option<u32>,
        text: String,
    },
    AsrDelta {
        #[serde(rename = "trackId")]
        track_id: String,
        index: u32,
        timestamp: u64,
        #[serde(rename = "startTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        start_time: Option<u32>,
        #[serde(rename = "endTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        end_time: Option<u32>,
        text: String,
    },
    /// timestamp, text
    LLMFinal {
        timestamp: u64,
        text: String,
    },
    /// track_id, timestamp,  word
    LLMDelta {
        timestamp: u64,
        word: String,
    },
    /// timestamp, metrics
    Metrics {
        timestamp: u64,
        sender: String,
        metrics: serde_json::Value,
    },
    /// timestamp, error message
    Error {
        timestamp: u64,
        error: String,
    },
}

pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;

pub fn create_event_sender() -> EventSender {
    EventSender::new(128)
}
