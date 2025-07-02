use crate::{
    media::{recorder::RecorderOption, vad::VADOption},
    synthesis::SynthesisOption,
    transcription::TranscriptionOption,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod call;
pub mod handler;
pub mod llmproxy;
pub mod middleware;
pub mod sip;
#[cfg(test)]
mod tests;
pub mod webrtc;
pub use handler::router;
use sip::SipOption;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CallOption {
    #[serde(skip_serializing_if = "Option::is_none")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>, // pcmu, pcma, g722, pcm, only for websocket call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eou: Option<EouOption>,
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
            codec: None,
            eou: None,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EouOption {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_id: Option<String>,
    /// max timeout in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
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
        /// If the play_id is the same, it will not interrupt the previous playback
        play_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "autoHangup")]
        /// If auto_hangup is true, it means the call will be hung up automatically after the TTS playback is finished
        auto_hangup: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        /// If streaming is true, it means the input text is streaming text,
        /// and end_of_stream needs to be used to determine if it's finished,
        /// equivalent to LLM's streaming output to TTS synthesis
        streaming: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        /// If end_of_stream is true, it means the input text is finished
        end_of_stream: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        option: Option<SynthesisOption>,
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
