use crate::{
    media::{recorder::RecorderConfig, vad::VADConfig},
    synthesis::SynthesisConfig,
    transcription::TranscriptionConfig,
};
use serde::{Deserialize, Serialize};

pub mod call;
pub mod handler;
pub mod llmproxy;
pub mod middleware;
pub mod processor;
pub mod sip;
#[cfg(test)]
mod tests;
pub mod webrtc;
pub use handler::router;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOption {
    pub denoise: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder: Option<RecorderConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vad: Option<VADConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr: Option<TranscriptionConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<SynthesisConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_ipv6: Option<bool>,
}

impl Default for StreamOption {
    fn default() -> Self {
        Self {
            denoise: None,
            offer: None,
            callee: None,
            caller: None,
            recorder: None,
            asr: None,
            vad: None,
            tts: None,
            handshake_timeout: None,
            enable_ipv6: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferOption {
    #[serde(skip_serializing_if = "Option::is_none")]
    bypass: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Music on hold
    moh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_hangup: Option<bool>,
}

// WebSocket Commands
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command")]
#[serde(rename_all = "camelCase")]
pub enum Command {
    Invite {
        options: StreamOption,
    },
    Accept {
        options: StreamOption,
    },
    Reject {
        reason: String,
    },
    Candidate {
        candidates: Vec<String>,
    },
    Tts {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "playId")]
        play_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "autoHangup")]
        auto_hangup: Option<bool>,
    },
    Play {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "autoHangup")]
        auto_hangup: Option<bool>,
    },
    Interrupt {},
    Pause {},
    Resume {},
    Hangup {
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        initiator: Option<String>,
    },
    Refer {
        target: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<ReferOption>,
    },
    Mute {
        #[serde(rename = "trackId")]
        track_id: Option<String>,
    },
    Unmute {
        #[serde(rename = "trackId")]
        track_id: Option<String>,
    },
}
