use crate::{
    media::{recorder::RecorderOption, vad::VADOption},
    synthesis::SynthesisOption,
    transcription::TranscriptionOption,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod active_call;
pub mod sip;
pub use active_call::ActiveCall;
pub use active_call::ActiveCallRef;
pub use active_call::ActiveCallState;
pub use active_call::ActiveCallType;

pub type CommandSender = tokio::sync::broadcast::Sender<Command>;
pub type CommandReceiver = tokio::sync::broadcast::Receiver<Command>;

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[serde(default)]
pub struct SipOption {
    pub username: String,
    pub password: String,
    pub realm: String,
    pub headers: Option<HashMap<String, String>>,
}

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

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReferOption {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denoise: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moh: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr: Option<TranscriptionOption>,
    /// hangup after the call is ended
    pub auto_hangup: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip: Option<SipOption>,
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
#[derive(Debug, Deserialize, Serialize, Clone)]
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
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "waitInputTimeout")]
        wait_input_timeout: Option<u32>,
    },
    Play {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "autoHangup")]
        auto_hangup: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "waitInputTimeout")]
        wait_input_timeout: Option<u32>,
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
        caller: String,
        /// aor of the calee, e.g., sip:bob@restsend.com
        callee: String,
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
