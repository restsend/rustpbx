use crate::{
    config::{MediaProxyMode, RouteResult},
    media::{recorder::RecorderOption, track::media_pass::MediaPassOption, vad::VADOption},
    synthesis::SynthesisOption,
    transcription::TranscriptionOption,
};
use anyhow::Result;
use rsip::{StatusCode, Transport};
use rsipstack::{
    dialog::{authenticate::Credential, invitation::InviteOption},
    transport::SipAddr,
};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
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
    pub username: Option<String>,
    pub password: Option<String>,
    pub realm: Option<String>,
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
    pub fn check_default(&mut self) {
        if let Some(tts) = &mut self.tts {
            tts.check_default();
        }
        if let Some(asr) = &mut self.asr {
            asr.check_default();
        }
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
                username: sip.username.clone().unwrap_or_default(),
                password: sip.password.clone().unwrap_or_default(),
                realm: sip.realm.clone(),
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
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
        wait_input_timeout: Option<u32>,
        /// if true, the text is base64 encoded pcm samples
        base64: Option<bool>,
    },
    Play {
        url: String,
        auto_hangup: Option<bool>,
        wait_input_timeout: Option<u32>,
    },
    Interrupt {
        graceful: Option<bool>,
    },
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

#[derive(Clone, Default)]
pub struct Location {
    pub aor: rsip::Uri,
    pub expires: u32,
    pub destination: Option<SipAddr>,
    pub last_modified: Option<Instant>,
    pub supports_webrtc: bool,
    pub credential: Option<Credential>,
    pub headers: Option<Vec<rsip::Header>>,
    pub registered_aor: Option<rsip::Uri>,
    pub contact_raw: Option<String>,
    pub contact_params: Option<HashMap<String, String>>,
    pub path: Option<Vec<rsip::Uri>>,
    pub service_route: Option<Vec<rsip::Uri>>,
    pub instance_id: Option<String>,
    pub gruu: Option<String>,
    pub temp_gruu: Option<String>,
    pub reg_id: Option<String>,
    pub transport: Option<Transport>,
    pub user_agent: Option<String>,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let is_webrtc = if self.supports_webrtc { ",webrtc" } else { "" };
        let fallback = self.aor.to_string();
        let contact = self.contact_raw.as_deref().unwrap_or(fallback.as_str());
        match &self.destination {
            Some(d) => write!(f, "({} -> {} {})", contact, d, is_webrtc),
            None => write!(f, "({} -> ? {})", contact, is_webrtc),
        }
    }
}

impl Location {
    pub fn binding_key(&self) -> String {
        if let Some(instance) = &self.instance_id {
            return format!("{}|instance={}", self.aor, instance);
        }
        if let Some(gruu) = &self.gruu {
            return format!("{}|gruu={}", self.aor, gruu);
        }
        self.aor.to_string()
    }

    pub fn is_expired_at(&self, now: Instant) -> bool {
        if self.expires == 0 {
            return true;
        }
        if let Some(last_modified) = self.last_modified {
            let ttl = std::time::Duration::from_secs(self.expires as u64);
            return now.duration_since(last_modified) >= ttl;
        }
        false
    }
}

#[derive(Clone)]
pub enum DialStrategy {
    Sequential(Vec<Location>),
    Parallel(Vec<Location>),
}

impl std::fmt::Display for DialStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialStrategy::Sequential(locations) => {
                write!(
                    f,
                    "Sequential: [{}]",
                    locations
                        .iter()
                        .map(|l| l.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            DialStrategy::Parallel(locations) => {
                write!(
                    f,
                    "Parallel: [{}]",
                    locations
                        .iter()
                        .map(|l| l.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RingbackConfig {
    pub audio_file: Option<String>,
    /// Whether to wait for ringtone playback completion before starting call dialing (default: false)
    pub wait_for_completion: Option<bool>,
}

impl Default for RingbackConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl RingbackConfig {
    pub fn new() -> Self {
        Self {
            audio_file: None,
            wait_for_completion: Some(false),
        }
    }

    pub fn with_audio_file(mut self, file: String) -> Self {
        self.audio_file = Some(file);
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
    Hangup(Option<StatusCode>),
    /// Play audio file and then hangup
    PlayThenHangup {
        audio_file: String,
        status_code: StatusCode,
    },
    /// Transfer to another destination
    Transfer(String),
}

impl Default for FailureAction {
    fn default() -> Self {
        Self::Hangup(None)
    }
}

/// Media configuration for call control
#[derive(Debug, Clone, Default)]
pub struct MediaConfig {
    /// Media proxy mode
    pub proxy_mode: MediaProxyMode,
    pub external_ip: Option<String>,
    pub rtp_start_port: Option<u16>,
    pub rtp_end_port: Option<u16>,
}

impl MediaConfig {
    pub fn new() -> Self {
        Self {
            proxy_mode: MediaProxyMode::Auto,
            external_ip: None,
            rtp_start_port: None,
            rtp_end_port: None,
        }
    }

    pub fn with_proxy_mode(mut self, mode: MediaProxyMode) -> Self {
        self.proxy_mode = mode;
        self
    }

    pub fn with_external_ip(mut self, ip: Option<String>) -> Self {
        self.external_ip = ip;
        self
    }

    pub fn with_rtp_start_port(mut self, start: Option<u16>) -> Self {
        self.rtp_start_port = start;
        self
    }
    pub fn with_rtp_end_port(mut self, end: Option<u16>) -> Self {
        self.rtp_end_port = end;
        self
    }
}

#[derive(Debug)]
pub enum DialDirection {
    Outbound, // 1. Outbound call initiated by us, usually to a PSTN gateway or another relay server
    Inbound,  // 2. Inbound call received by us, usually from a PSTN gateway
    Internal, // 3. User to user call, both sides are internal
}
pub struct Dialplan {
    pub direction: DialDirection,
    pub session_id: Option<String>,
    pub caller_contact: Option<rsip::typed::Contact>,
    pub caller_display_name: Option<String>,
    pub caller: Option<rsip::Uri>,
    pub targets: DialStrategy,
    pub max_ring_time: u32,
    pub original: rsip::Request,
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

    pub route_invite: Option<Box<dyn RouteInvite>>,
    pub with_original_headers: bool,
}

impl Dialplan {
    pub fn is_empty(&self) -> bool {
        match &self.targets {
            DialStrategy::Sequential(targets) => targets.is_empty(),
            DialStrategy::Parallel(targets) => targets.is_empty(),
        }
    }
    pub fn all_webrtc_target(&self) -> bool {
        match &self.targets {
            DialStrategy::Sequential(targets) => targets.iter().all(|loc| loc.supports_webrtc),
            DialStrategy::Parallel(targets) => targets.iter().all(|loc| loc.supports_webrtc),
        }
    }
    /// Create a new dialplan with basic configuration
    pub fn new(session_id: String, original: rsip::Request, direction: DialDirection) -> Self {
        Self {
            direction,
            session_id: Some(session_id),
            original,
            caller_display_name: None,
            caller: None,
            caller_contact: None,
            targets: DialStrategy::Sequential(vec![]),
            max_ring_time: 60,
            recording: CallRecordingConfig::default(),
            ringback: RingbackConfig::default(),
            media: MediaConfig::default(),
            max_call_duration: Some(Duration::from_secs(3600)), // 1 hour
            call_timeout: Duration::from_secs(60),              // 60 seconds
            failure_action: FailureAction::default(),
            route_invite: None,
            with_original_headers: true,
        }
    }

    /// Set the caller URI
    pub fn with_caller(mut self, caller: rsip::Uri) -> Self {
        self.caller = Some(caller);
        self
    }
    pub fn with_targets(mut self, targets: DialStrategy) -> Self {
        self.targets = targets;
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
    pub fn with_route_invite(mut self, route: Box<dyn RouteInvite>) -> Self {
        self.route_invite = Some(route);
        self
    }

    pub fn with_caller_contact(mut self, contact: rsip::typed::Contact) -> Self {
        self.caller_contact = Some(contact);
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
}

#[async_trait::async_trait]
pub trait RouteInvite: Sync + Send {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsip::Request,
        direction: &DialDirection,
    ) -> Result<RouteResult>;
}

/// Routing state for managing stateful load balancing
#[derive(Debug)]
pub struct RoutingState {
    /// Round-robin counters for each destination group
    round_robin_counters: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for RoutingState {
    fn default() -> Self {
        Self::new()
    }
}

impl RoutingState {
    pub fn new() -> Self {
        Self {
            round_robin_counters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get the next trunk index for round-robin selection
    pub fn next_round_robin_index(&self, destination_key: &str, trunk_count: usize) -> usize {
        if trunk_count == 0 {
            return 0;
        }

        let mut counters = self.round_robin_counters.lock().unwrap();
        let counter = counters
            .entry(destination_key.to_string())
            .or_insert_with(|| 0);
        let r = *counter % trunk_count;
        *counter += 1;
        return r;
    }
}
