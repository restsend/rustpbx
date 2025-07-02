use crate::PcmBuf;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<u32>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        initiator: Option<String>,
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
    ///End of Utterance
    Eou {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        completed: bool,
    },
    Dtmf {
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
        duration: u64,
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
        start_time: Option<u64>,
        #[serde(rename = "endTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        end_time: Option<u64>,
        text: String,
    },
    AsrDelta {
        #[serde(rename = "trackId")]
        track_id: String,
        index: u32,
        timestamp: u64,
        #[serde(rename = "startTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        start_time: Option<u64>,
        #[serde(rename = "endTime")]
        #[serde(skip_serializing_if = "Option::is_none")]
        end_time: Option<u64>,
        text: String,
    },
    Metrics {
        timestamp: u64,
        key: String,
        duration: u32,
        data: serde_json::Value,
    },
    Error {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        sender: String,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<u32>,
    },
    AddHistory {
        sender: Option<String>,
        timestamp: u64,
        speaker: String,
        text: String,
    },
    Other {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        sender: String,
        extra: Option<HashMap<String, String>>,
    },
    Binary {
        #[serde(rename = "trackId")]
        track_id: String,
        timestamp: u64,
        data: Vec<u8>,
    },
}

pub type EventSender = tokio::sync::broadcast::Sender<SessionEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<SessionEvent>;

pub fn create_event_sender() -> EventSender {
    EventSender::new(128)
}
