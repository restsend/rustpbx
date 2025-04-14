use crate::PcmBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "camelCase")]
pub enum SessionEvent {
    Incoming {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        caller: String,
        callee: String,
        sdp: String,
    },
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
        initiator: String,
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
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        #[serde(rename = "startTime")]
        start_time: u64,
        r#type: String, // loud, music, unclear
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
    Interruption {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        position: u64, // current playback position at the time of interruption
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
    LLMFinal { timestamp: u64, text: String },
    /// track_id, timestamp,  word
    LLMDelta { timestamp: u64, word: String },
    /// timestamp, metrics
    Metrics {
        timestamp: u64,
        sender: String,
        metrics: serde_json::Value,
    },
    /// timestamp, error message
    Error {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        sender: String,
        error: String,
    },
}

pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;

pub fn create_event_sender() -> EventSender {
    EventSender::new(128)
}
