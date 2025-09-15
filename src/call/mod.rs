use crate::{
    config::RouteResult,
    media::{recorder::RecorderOption, track::media_pass::MediaPassOption, vad::VADOption},
    synthesis::SynthesisOption,
    transcription::TranscriptionOption,
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::{
    dialog::{authenticate::Credential, invitation::InviteOption},
    transport::SipAddr,
};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{collections::HashMap, time::Instant};
pub mod active_call;
pub mod b2bua;
pub mod cookie;
pub mod sip;
pub mod user;
pub use active_call::ActiveCall;
pub use active_call::ActiveCallRef;
pub use active_call::ActiveCallState;
pub use active_call::ActiveCallType;
pub use cookie::TransactionCookie;
pub use user::SipUser;

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

#[skip_serializing_none]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CallOption {
    pub denoise: Option<bool>,
    pub offer: Option<String>,
    pub callee: Option<String>,
    pub caller: Option<String>,
    pub recorder: Option<RecorderOption>,
    pub vad: Option<VADOption>,
    pub asr: Option<TranscriptionOption>,
    pub tts: Option<SynthesisOption>,
    pub media_pass: Option<MediaPassOption>,
    pub handshake_timeout: Option<String>,
    pub enable_ipv6: Option<bool>,
    pub sip: Option<SipOption>,
    pub extra: Option<HashMap<String, String>>,
    pub codec: Option<String>, // pcmu, pcma, g722, pcm, only for websocket call
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
            media_pass: None,
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

    pub fn build_invite_option(&self) -> Result<InviteOption> {
        let mut invite_option = InviteOption::default();
        if let Some(offer) = &self.offer {
            invite_option.offer = Some(offer.clone().into());
        }
        if let Some(callee) = &self.callee {
            invite_option.callee = callee.clone().try_into()?;
        }
        if let Some(caller) = &self.caller {
            invite_option.caller = caller.clone().try_into()?;
            invite_option.contact = invite_option.caller.clone();
        }

        if let Some(sip) = &self.sip {
            invite_option.credential = Some(Credential {
                username: sip.username.clone(),
                password: sip.password.clone(),
                realm: Some(sip.realm.clone()),
            });
            invite_option.headers = sip.headers.as_ref().map(|h| {
                h.iter()
                    .map(|(k, v)| rsip::Header::Other(k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            });
        }
        Ok(invite_option)
    }
}

#[skip_serializing_none]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ReferOption {
    pub denoise: Option<bool>,
    pub timeout: Option<u32>,
    pub moh: Option<String>,
    pub asr: Option<TranscriptionOption>,
    /// hangup after the call is ended
    pub auto_hangup: Option<bool>,
    pub sip: Option<SipOption>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EouOption {
    pub r#type: Option<String>,
    pub endpoint: Option<String>,
    pub secret_key: Option<String>,
    pub secret_id: Option<String>,
    /// max timeout in milliseconds
    pub timeout: Option<u32>,
}

// WebSocket Commands
#[skip_serializing_none]
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(
    tag = "command",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
    Ringing {
        recorder: Option<RecorderOption>,
        early_media: Option<bool>,
        ringtone: Option<String>,
    },
    Tts {
        text: String,
        speaker: Option<String>,
        /// If the play_id is the same, it will not interrupt the previous playback
        play_id: Option<String>,
        /// If auto_hangup is true, it means the call will be hung up automatically after the TTS playback is finished
        auto_hangup: Option<bool>,
        /// If streaming is true, it means the input text is streaming text,
        /// and end_of_stream needs to be used to determine if it's finished,
        /// equivalent to LLM's streaming output to TTS synthesis
        streaming: Option<bool>,
        /// If end_of_stream is true, it means the input text is finished
        #[serde(rename = "endOfStream")]
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
        wait_input_timeout: Option<u32>,
    },
    Play {
        url: String,
        auto_hangup: Option<bool>,
        wait_input_timeout: Option<u32>,
    },
    Interrupt {},
    Pause {},
    Resume {},
    Hangup {
        reason: Option<String>,
        initiator: Option<String>,
    },
    Refer {
        caller: String,
        /// aor of the calee, e.g., sip:bob@restsend.com
        callee: String,
        options: Option<ReferOption>,
    },
    Mute {
        track_id: Option<String>,
    },
    Unmute {
        track_id: Option<String>,
    },
    History {
        speaker: String,
        text: String,
    },
}

#[async_trait]
pub trait LocationInspector: Send + Sync {
    async fn inspect_location(
        &self,
        location: Location,
        original: &rsip::Request,
    ) -> Result<Location, (anyhow::Error, Option<rsip::StatusCode>)>;
}

#[derive(Clone, Default)]
pub struct Location {
    pub aor: rsip::Uri,
    pub expires: u32,
    pub destination: SipAddr,
    pub last_modified: Option<Instant>,
    pub supports_webrtc: bool,
    pub credential: Option<Credential>,
    pub headers: Option<Vec<rsip::Header>>,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aor: {}, destination: {}", self.aor, self.destination)
    }
}

#[derive(Clone)]
pub enum DialStrategy {
    Sequential(Vec<Location>),
    Parallel(Vec<Location>),
}

pub struct Dialplan {
    pub session_id: Option<String>,
    pub caller: Option<rsip::Uri>,
    pub targets: DialStrategy,
    pub max_ring_time: u32,
    pub route_invite: Option<Box<dyn RouteInvite>>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
}

impl Dialplan {
    pub fn is_empty(&self) -> bool {
        match &self.targets {
            DialStrategy::Sequential(targets) => targets.is_empty(),
            DialStrategy::Parallel(targets) => targets.is_empty(),
        }
    }
}

impl Default for Dialplan {
    fn default() -> Self {
        Self {
            session_id: None,
            caller: None,
            targets: DialStrategy::Sequential(vec![]),
            max_ring_time: 60,
            route_invite: None,
            extras: None,
        }
    }
}

#[async_trait::async_trait]
pub trait RouteInvite: Sync + Send {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsip::Request,
    ) -> Result<RouteResult>;
}
