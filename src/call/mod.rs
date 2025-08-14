use crate::{
    config::RouteResult,
    media::{media_pass::MediaPassOption, recorder::RecorderOption, vad::VADOption},
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_pass: Option<MediaPassOption>,
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
            media_pass: None,
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
    Ringing {
        recorder: bool,
        early_media: bool,
        ringtone: Option<String>,
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

#[derive(Clone)]
pub enum DialStrategy {
    Sequential(Vec<Location>),
    Parallel(Vec<Location>),
}

pub struct Dialplan {
    pub targets: DialStrategy,
    pub max_ring_time: u32,
    pub route_invite: Option<Box<dyn RouteInvite>>,
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
            targets: DialStrategy::Sequential(vec![]),
            max_ring_time: 60,
            route_invite: None,
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
