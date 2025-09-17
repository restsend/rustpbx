use crate::{
    config::{MediaProxyMode, RouteResult},
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
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
pub mod active_call;
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

impl std::fmt::Debug for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Location")
            .field("aor", &self.aor)
            .field("expires", &self.expires)
            .field("destination", &self.destination)
            .field("last_modified", &self.last_modified)
            .field("supports_webrtc", &self.supports_webrtc)
            .field("credential", &"<Credential>")
            .field("headers", &self.headers)
            .finish()
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aor: {}, destination: {}", self.aor, self.destination)
    }
}

#[derive(Clone, Debug)]
pub enum DialStrategy {
    Sequential(Vec<Location>),
    Parallel(Vec<Location>),
}

/// Ringback configuration for call control
#[derive(Debug, Clone)]
pub struct RingbackConfig {
    /// Play ringback for these status codes
    pub status_codes: Vec<u16>,
    /// Audio file path for ringback tone
    pub audio_file: Option<String>,
    /// Maximum ringback duration
    pub max_duration: Option<Duration>,
    /// Auto hangup after ringback finishes
    pub auto_hangup: Option<bool>,
    /// Status code to send when auto hanging up
    pub hangup_status_code: Option<u16>,
}

impl Default for RingbackConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl RingbackConfig {
    pub fn new() -> Self {
        Self {
            status_codes: vec![180, 183], // Ringing, Session Progress
            audio_file: None,
            max_duration: Some(Duration::from_secs(60)),
            auto_hangup: None,
            hangup_status_code: None,
        }
    }

    pub fn with_audio_file(mut self, file: String) -> Self {
        self.audio_file = Some(file);
        self
    }

    pub fn with_auto_hangup(mut self, status_code: u16) -> Self {
        self.auto_hangup = Some(true);
        self.hangup_status_code = Some(status_code);
        self
    }
}

/// Recording configuration for call control
#[derive(Debug, Clone, Default)]
pub struct CallRecordingConfig {
    /// Enable call recording
    pub enabled: bool,
    /// Recording configuration
    pub recorder_config: Option<RecorderOption>,
    /// Auto start recording when call is answered
    pub auto_start: bool,
    /// Recording file name pattern
    pub filename_pattern: Option<String>,
}

impl CallRecordingConfig {
    pub fn new() -> Self {
        Self {
            enabled: false,
            recorder_config: None,
            auto_start: true,
            filename_pattern: None,
        }
    }

    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }

    pub fn with_config(mut self, config: RecorderOption) -> Self {
        self.recorder_config = Some(config);
        self
    }
}

/// Failure handling strategy for call control
#[derive(Debug, Clone)]
pub enum FailureAction {
    /// Hangup with specific status code
    Hangup(u16),
    /// Play audio file and then hangup
    PlayThenHangup {
        audio_file: String,
        status_code: u16,
    },
    /// Transfer to another destination
    Transfer(String),
    /// Try next target in sequence (only for Sequential strategy)
    TryNext,
}

impl Default for FailureAction {
    fn default() -> Self {
        Self::Hangup(486) // Busy Here
    }
}

/// Media configuration for call control
#[derive(Debug, Clone, Default)]
pub struct MediaConfig {
    /// Media proxy mode
    pub proxy_mode: MediaProxyMode,
    /// Enable media pass-through
    pub media_pass: Option<MediaPassOption>,
    /// Voice activity detection
    pub vad: Option<VADOption>,
    /// Audio enhancement (denoise)
    pub denoise: Option<bool>,
    /// Transcription (ASR) configuration
    pub asr: Option<TranscriptionOption>,
    /// Text-to-speech configuration
    pub tts: Option<SynthesisOption>,
}

impl MediaConfig {
    pub fn new() -> Self {
        Self {
            proxy_mode: MediaProxyMode::Auto,
            media_pass: None,
            vad: None,
            denoise: None,
            asr: None,
            tts: None,
        }
    }

    pub fn with_proxy_mode(mut self, mode: MediaProxyMode) -> Self {
        self.proxy_mode = mode;
        self
    }

    pub fn with_asr(mut self, asr: TranscriptionOption) -> Self {
        self.asr = Some(asr);
        self
    }

    pub fn with_tts(mut self, tts: SynthesisOption) -> Self {
        self.tts = Some(tts);
        self
    }
}

pub struct Dialplan {
    pub session_id: Option<String>,
    pub caller: Option<rsip::Uri>,
    pub targets: DialStrategy,
    pub max_ring_time: u32,

    // Enhanced call control options
    /// Recording configuration
    pub recording: CallRecordingConfig,
    /// Ringback configuration
    pub ringback: RingbackConfig,
    /// Media configuration
    pub media: MediaConfig,
    /// Maximum call duration
    pub max_call_duration: Option<Duration>,
    /// Call timeout for individual legs
    pub call_timeout: Duration,
    /// What to do when a call fails
    pub failure_action: FailureAction,

    // Legacy fields (不参与 Clone)
    pub route_invite: Option<Box<dyn RouteInvite>>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
}

impl std::fmt::Debug for Dialplan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialplan")
            .field("session_id", &self.session_id)
            .field("caller", &self.caller)
            .field("targets", &self.targets)
            .field("max_ring_time", &self.max_ring_time)
            .field("recording", &self.recording)
            .field("ringback", &self.ringback)
            .field("media", &self.media)
            .field("max_call_duration", &self.max_call_duration)
            .field("call_timeout", &self.call_timeout)
            .field("failure_action", &self.failure_action)
            .field("route_invite", &"<RouteInvite trait object>")
            .field("extras", &self.extras)
            .finish()
    }
}

impl Clone for Dialplan {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            caller: self.caller.clone(),
            targets: self.targets.clone(),
            max_ring_time: self.max_ring_time,
            recording: self.recording.clone(),
            ringback: self.ringback.clone(),
            media: self.media.clone(),
            max_call_duration: self.max_call_duration,
            call_timeout: self.call_timeout,
            failure_action: self.failure_action.clone(),
            route_invite: None, // 不克隆 RouteInvite trait object
            extras: self.extras.clone(),
        }
    }
}

impl Dialplan {
    pub fn is_empty(&self) -> bool {
        match &self.targets {
            DialStrategy::Sequential(targets) => targets.is_empty(),
            DialStrategy::Parallel(targets) => targets.is_empty(),
        }
    }

    /// Create a new dialplan with basic configuration
    pub fn new(session_id: String) -> Self {
        Self {
            session_id: Some(session_id),
            ..Default::default()
        }
    }

    /// Set the caller URI
    pub fn with_caller(mut self, caller: rsip::Uri) -> Self {
        self.caller = Some(caller);
        self
    }

    /// Set targets with sequential strategy
    pub fn with_sequential_targets(mut self, targets: Vec<Location>) -> Self {
        self.targets = DialStrategy::Sequential(targets);
        self
    }

    /// Set targets with parallel strategy
    pub fn with_parallel_targets(mut self, targets: Vec<Location>) -> Self {
        self.targets = DialStrategy::Parallel(targets);
        self
    }

    /// Configure recording
    pub fn with_recording(mut self, recording: CallRecordingConfig) -> Self {
        self.recording = recording;
        self
    }

    /// Configure ringback
    pub fn with_ringback(mut self, ringback: RingbackConfig) -> Self {
        self.ringback = ringback;
        self
    }

    /// Configure media
    pub fn with_media(mut self, media: MediaConfig) -> Self {
        self.media = media;
        self
    }

    /// Set failure action
    pub fn with_failure_action(mut self, action: FailureAction) -> Self {
        self.failure_action = action;
        self
    }

    /// Set call timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Get all target locations regardless of strategy
    pub fn get_all_targets(&self) -> &Vec<Location> {
        match &self.targets {
            DialStrategy::Sequential(targets) => targets,
            DialStrategy::Parallel(targets) => targets,
        }
    }

    /// Check if using parallel dialing strategy
    pub fn is_parallel_strategy(&self) -> bool {
        matches!(self.targets, DialStrategy::Parallel(_))
    }

    /// Check if recording is enabled
    pub fn is_recording_enabled(&self) -> bool {
        self.recording.enabled
    }

    /// Check if auto hangup after ringback is enabled
    pub fn is_auto_hangup_enabled(&self) -> bool {
        self.ringback.auto_hangup.unwrap_or(false)
    }
}

impl Default for Dialplan {
    fn default() -> Self {
        Self {
            session_id: None,
            caller: None,
            targets: DialStrategy::Sequential(vec![]),
            max_ring_time: 60,
            recording: CallRecordingConfig::default(),
            ringback: RingbackConfig::default(),
            media: MediaConfig::default(),
            max_call_duration: Some(Duration::from_secs(3600)), // 1 hour
            call_timeout: Duration::from_secs(60),              // 60 seconds
            failure_action: FailureAction::default(),
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
