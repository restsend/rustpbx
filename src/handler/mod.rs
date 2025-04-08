use crate::{
    media::{recorder::RecorderConfig, vad::VADConfig},
    synthesis::SynthesisConfig,
    transcription::TranscriptionConfig,
};
use serde::{Deserialize, Serialize};

pub mod call;
pub mod handler;
pub mod processor;
pub mod sip;
#[cfg(test)]
mod tests;
pub mod webrtc;
pub use handler::router;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
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
}

impl Default for StreamOptions {
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
        }
    }
}

// WebSocket Commands
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command")]
pub enum Command {
    /// Invite a call
    #[serde(rename = "invite")]
    Invite { options: StreamOptions },
    /// Update the candidate for WebRTC
    #[serde(rename = "candidate")]
    Candidate { candidates: Vec<String> },
    /// Play a text to speech
    #[serde(rename = "tts")]
    Tts {
        text: String,
        speaker: Option<String>,
        play_id: Option<String>,
    },
    /// Play a wav file
    #[serde(rename = "play")]
    Play { url: String },
    /// Hangup the call
    #[serde(rename = "hangup")]
    Hangup {},
    /// Refer to a target
    #[serde(rename = "refer")]
    Refer { target: String },
    /// Mute a track
    #[serde(rename = "mute")]
    Mute { track_id: Option<String> },
    /// Unmute a track
    #[serde(rename = "unmute")]
    Unmute { track_id: Option<String> },
}
