use std::collections::HashMap;

use crate::{
    media::{recorder::RecorderOption, vad::VADOption},
    synthesis::SynthesisOption,
    transcription::TranscriptionOption,
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
use sip::SipOption;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CallOption {
    pub denoise: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorder: Option<RecorderOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vad: Option<VADOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr: Option<TranscriptionOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<SynthesisOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_ipv6: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip: Option<SipOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<HashMap<String, String>>,
}

impl Default for CallOption {
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
            sip: None,
            extra: None,
        }
    }
}

impl CallOption {
    pub fn check_default(&mut self) -> &CallOption {
        if let Some(tts) = &mut self.tts {
            tts.check_default();
        }
        if let Some(asr) = &mut self.asr {
            asr.check_default();
        }
        self
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
        option: CallOption,
    },
    Accept {
        option: CallOption,
    },
    Reject {
        reason: String,
        code: Option<u32>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        streaming: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        end_of_stream: Option<bool>,
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
    History {
        speaker: String,
        text: String,
    },
}
