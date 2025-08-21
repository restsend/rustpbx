use crate::PcmBuf;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    pub username: String,
    pub realm: String,
    pub source: String,
}

impl From<&String> for Attendee {
    fn from(source: &String) -> Self {
        let uri = rsip::Uri::try_from(source.as_str()).unwrap_or_default();
        Self {
            username: uri.user().unwrap_or_default().to_string(),
            realm: uri.host().to_string(),
            source: source.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    Incoming {
        track_id: String,
        timestamp: u64,
        caller: String,
        callee: String,
        sdp: String,
    },
    Answer {
        track_id: String,
        timestamp: u64,
        sdp: String,
    },
    Reject {
        track_id: String,
        timestamp: u64,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<u32>,
    },
    Ringing {
        track_id: String,
        timestamp: u64,
        early_media: bool,
    },
    Hangup {
        timestamp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        initiator: Option<String>,
        start_time: String,
        hangup_time: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        answer_time: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ringing_time: Option<String>,
        from: Option<Attendee>,
        to: Option<Attendee>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extra: Option<HashMap<String, serde_json::Value>>,
    },
    AnswerMachineDetection {
        // Answer machine detection
        timestamp: u64,
        start_time: u64,
        end_time: u64,
        text: String,
    },
    Speaking {
        track_id: String,
        timestamp: u64,
        start_time: u64,
    },
    Silence {
        track_id: String,
        timestamp: u64,
        start_time: u64,
        duration: u64,
        #[serde(skip)]
        samples: Option<PcmBuf>,
    },
    ///End of Utterance
    Eou {
        track_id: String,
        timestamp: u64,
        completed: bool,
    },
    Dtmf {
        track_id: String,
        timestamp: u64,
        digit: String,
    },
    TrackStart {
        track_id: String,
        timestamp: u64,
    },
    TrackEnd {
        track_id: String,
        timestamp: u64,
        duration: u64,
        ssrc: u32,
    },
    Interruption {
        track_id: String,
        timestamp: u64,
        subtitle: Option<String>, // current tts text
        position: Option<u32>,    // word index in subtitle
        total_duration: u32,      // whole tts duration
        current: u32,             // elapsed time since start of tts
    },
    AsrFinal {
        track_id: String,
        timestamp: u64,
        index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        start_time: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        end_time: Option<u64>,
        text: String,
    },
    AsrDelta {
        track_id: String,
        index: u32,
        timestamp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        start_time: Option<u64>,
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
        track_id: String,
        timestamp: u64,
        sender: String,
        extra: Option<HashMap<String, String>>,
    },
    Binary {
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
