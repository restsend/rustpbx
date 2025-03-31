use crate::{
    media::vad::{VADConfig, VadType},
    synthesis::{SynthesisConfig, SynthesisType},
    transcription::{TranscriptionConfig, TranscriptionType},
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
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_recorder: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vad_type: Option<VadType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vad_config: Option<VADConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_type: Option<TranscriptionType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asr_config: Option<TranscriptionConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_type: Option<SynthesisType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_config: Option<SynthesisConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handshake_timeout: Option<String>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            sdp: None,
            callee: None,
            caller: None,
            enable_recorder: Some(true),
            vad_type: Some(VadType::WebRTC),
            vad_config: None,
            asr_type: Some(TranscriptionType::TencentCloud),
            asr_config: None,
            tts_type: Some(SynthesisType::TencentCloud),
            tts_config: None,
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
