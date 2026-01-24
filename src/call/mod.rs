use crate::{
    config::{MediaProxyMode, RouteResult},
    media::recorder::RecorderOption,
};
use anyhow::Result;
use audio_codec::CodecType;
use rsip::{StatusCode, Transport};
use rsipstack::{
    dialog::{authenticate::Credential, invitation::InviteOption},
    transport::SipAddr,
};
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub mod cookie;
pub mod policy;
pub mod queue_config;
pub mod sip;
pub mod user;
pub use cookie::{CalleeDisplayName, TenantId, TransactionCookie, TrunkContext};
pub use user::SipUser;

/// Default hold audio that ships with config/sounds.
pub const DEFAULT_QUEUE_HOLD_AUDIO: &str = "config/sounds/phone-calling.wav";
/// Default prompt played when a queue cannot find an available agent.
pub const DEFAULT_QUEUE_FAILURE_AUDIO: &str = "config/sounds/unavailable-phone.wav";

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

impl std::fmt::Debug for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Location")
            .field("aor", &self.aor)
            .field("expires", &self.expires)
            .field("destination", &self.destination)
            .field("last_modified", &self.last_modified)
            .field("supports_webrtc", &self.supports_webrtc)
            .field("headers", &self.headers)
            .field("registered_aor", &self.registered_aor)
            .field("contact_raw", &self.contact_raw)
            .field("contact_params", &self.contact_params)
            .field("path", &self.path)
            .field("service_route", &self.service_route)
            .field("instance_id", &self.instance_id)
            .field("gruu", &self.gruu)
            .field("temp_gruu", &self.temp_gruu)
            .field("reg_id", &self.reg_id)
            .field("transport", &self.transport)
            .field("user_agent", &self.user_agent)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .finish()
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

#[derive(Clone, Debug)]
pub enum DialStrategy {
    Sequential(Vec<Location>),
    Parallel(Vec<Location>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEndpoint {
    Uri(String),
    Queue(String),
}

impl TransferEndpoint {
    pub fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }

        const QUEUE_PREFIX: &str = "queue:";

        if trimmed.len() >= QUEUE_PREFIX.len()
            && trimmed[..QUEUE_PREFIX.len()].eq_ignore_ascii_case(QUEUE_PREFIX)
        {
            let name = trimmed[QUEUE_PREFIX.len()..].trim();
            if name.is_empty() {
                return None;
            }
            // If name is numeric, it's likely an ID, but we store it as string in TransferEndpoint::Queue
            return Some(TransferEndpoint::Queue(name.to_string()));
        }
        Some(TransferEndpoint::Uri(trimmed.to_string()))
    }
}

impl std::fmt::Display for TransferEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferEndpoint::Uri(uri) => write!(f, "{}", uri),
            TransferEndpoint::Queue(name) => write!(f, "queue:{}", name),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallForwardingMode {
    Always,
    WhenBusy,
    WhenNoAnswer,
}

pub const CALL_FORWARDING_TIMEOUT_MIN_SECS: u64 = 5;
pub const CALL_FORWARDING_TIMEOUT_MAX_SECS: u64 = 120;
pub const CALL_FORWARDING_TIMEOUT_DEFAULT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct CallForwardingConfig {
    pub mode: CallForwardingMode,
    pub endpoint: TransferEndpoint,
    pub timeout: Duration,
}

impl CallForwardingConfig {
    pub fn new(mode: CallForwardingMode, endpoint: TransferEndpoint, timeout_secs: u64) -> Self {
        let clamped = timeout_secs.clamp(
            CALL_FORWARDING_TIMEOUT_MIN_SECS,
            CALL_FORWARDING_TIMEOUT_MAX_SECS,
        );
        let timeout = Duration::from_secs(clamped);
        Self {
            mode,
            endpoint,
            timeout,
        }
    }

    pub fn clamp_timeout(value: i64) -> u64 {
        if value <= 0 {
            return CALL_FORWARDING_TIMEOUT_DEFAULT_SECS;
        }
        value.clamp(
            CALL_FORWARDING_TIMEOUT_MIN_SECS as i64,
            CALL_FORWARDING_TIMEOUT_MAX_SECS as i64,
        ) as u64
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RingbackMode {
    Local,
    Passthrough,
    Auto,
    None,
}

impl Default for RingbackMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingbackConfig {
    #[serde(default)]
    pub mode: RingbackMode,
    pub audio_file: Option<String>,
    #[serde(default = "default_ringback_loop")]
    pub loop_playback: bool,
    #[serde(default)]
    pub wait_for_completion: bool,
}

fn default_ringback_loop() -> bool {
    true
}

impl Default for RingbackConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl RingbackConfig {
    pub fn new() -> Self {
        Self {
            mode: RingbackMode::Auto,
            audio_file: None,
            loop_playback: true,
            wait_for_completion: false,
        }
    }

    pub fn with_mode(mut self, mode: RingbackMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_audio_file(mut self, file: String) -> Self {
        self.audio_file = Some(file);
        self
    }

    pub fn with_loop(mut self, loop_playback: bool) -> Self {
        self.loop_playback = loop_playback;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueHoldConfig {
    pub audio_file: Option<String>,
    pub loop_playback: bool,
}

impl Default for QueueHoldConfig {
    fn default() -> Self {
        Self {
            audio_file: None,
            loop_playback: true,
        }
    }
}

impl QueueHoldConfig {
    pub fn with_audio_file(mut self, file: String) -> Self {
        self.audio_file = Some(file);
        self
    }

    pub fn with_loop_playback(mut self, loop_playback: bool) -> Self {
        self.loop_playback = loop_playback;
        self
    }
}

#[derive(Debug, Clone)]
pub enum QueueFallbackAction {
    /// Reuse existing failure behaviors
    Failure(FailureAction),
    /// Redirect to a specific SIP URI (e.g., external voicemail)
    Redirect { target: rsip::Uri },
    /// Transfer caller to another named queue
    Queue { name: String },
}

#[derive(Debug, Clone)]
pub struct QueuePlan {
    pub accept_immediately: bool,
    pub passthrough_ringback: bool,
    pub hold: Option<QueueHoldConfig>,
    pub fallback: Option<QueueFallbackAction>,
    pub dial_strategy: Option<DialStrategy>,
    pub ring_timeout: Option<Duration>,
    pub label: Option<String>,
    pub retry_codes: Option<Vec<u16>>,
    pub no_trying_timeout: Option<Duration>,
}

impl Default for QueuePlan {
    fn default() -> Self {
        Self {
            accept_immediately: false,
            passthrough_ringback: false,
            hold: Some(
                QueueHoldConfig::default().with_audio_file(DEFAULT_QUEUE_HOLD_AUDIO.to_string()),
            ),
            fallback: Some(QueueFallbackAction::Failure(
                FailureAction::PlayThenHangup {
                    audio_file: DEFAULT_QUEUE_FAILURE_AUDIO.to_string(),
                    use_early_media: false,
                    status_code: StatusCode::TemporarilyUnavailable,
                    reason: Some("All agents are currently unavailable".to_string()),
                },
            )),
            dial_strategy: None,
            ring_timeout: None,
            label: None,
            retry_codes: None,
            no_trying_timeout: None,
        }
    }
}

impl QueuePlan {
    pub fn dial_strategy(&self) -> Option<&DialStrategy> {
        self.dial_strategy.as_ref()
    }

    pub fn passthrough_ringback(&self) -> bool {
        self.passthrough_ringback
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

#[derive(Debug, Clone)]
pub enum DialplanFlow {
    Targets(DialStrategy),
    Queue {
        plan: QueuePlan,
        next: Box<DialplanFlow>,
    },
}

impl DialplanFlow {
    fn replace_terminal(current: DialplanFlow, new_terminal: DialplanFlow) -> DialplanFlow {
        match current {
            DialplanFlow::Queue { plan, next } => DialplanFlow::Queue {
                plan,
                next: Box::new(Self::replace_terminal(*next, new_terminal)),
            },
            _ => new_terminal,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            DialplanFlow::Targets(strategy) => match strategy {
                DialStrategy::Sequential(targets) | DialStrategy::Parallel(targets) => {
                    targets.is_empty()
                }
            },
            DialplanFlow::Queue { .. } => false,
        }
    }

    pub fn get_queue_plan_recursive(&self) -> Option<QueuePlan> {
        match self {
            DialplanFlow::Queue { plan, .. } => Some(plan.clone()),
            _ => None,
        }
    }

    fn all_webrtc_target(&self) -> bool {
        match self {
            DialplanFlow::Targets(strategy) => match strategy {
                DialStrategy::Sequential(targets) | DialStrategy::Parallel(targets) => {
                    targets.iter().all(|loc| loc.supports_webrtc)
                }
            },
            DialplanFlow::Queue { next, .. } => next.all_webrtc_target(),
        }
    }

    fn find_targets(&self) -> Option<&Vec<Location>> {
        match self {
            DialplanFlow::Targets(strategy) => match strategy {
                DialStrategy::Sequential(targets) | DialStrategy::Parallel(targets) => {
                    Some(targets)
                }
            },
            DialplanFlow::Queue { next, .. } => next.find_targets(),
        }
    }

    fn is_parallel(&self) -> bool {
        match self {
            DialplanFlow::Targets(DialStrategy::Parallel(_)) => true,
            DialplanFlow::Queue { next, .. } => next.is_parallel(),
            _ => false,
        }
    }

    fn has_queue(&self) -> bool {
        matches!(self, DialplanFlow::Queue { .. })
    }

    fn has_queue_hold_audio(&self) -> bool {
        match self {
            DialplanFlow::Queue { plan, next } => {
                let hold_audio = plan
                    .hold
                    .as_ref()
                    .and_then(|hold| hold.audio_file.as_ref())
                    .is_some();
                hold_audio || next.has_queue_hold_audio()
            }
            _ => false,
        }
    }
}
/// Recording configuration for call control
#[derive(Debug, Clone, Default)]
pub struct CallRecordingConfig {
    /// Enable call recording
    pub enabled: bool,
    /// Recording configuration
    pub option: Option<RecorderOption>,
    /// Auto start recording when call is answered
    pub auto_start: bool,
}

impl CallRecordingConfig {
    pub fn new() -> Self {
        Self {
            enabled: false,
            option: None,
            auto_start: true,
        }
    }

    pub fn enabled(mut self) -> Self {
        self.enabled = true;
        self
    }

    pub fn with_config(mut self, config: RecorderOption) -> Self {
        self.option = Some(config);
        self
    }
}

/// Failure handling strategy for call control
#[derive(Debug, Clone)]
pub enum FailureAction {
    /// Hangup with specific status code
    Hangup {
        code: Option<StatusCode>,
        reason: Option<String>,
    },

    /// Play audio file and then hangup
    PlayThenHangup {
        /// Audio file to play
        audio_file: String,

        /// Whether to use 183 early media (true) or 200 OK (false) for playback
        use_early_media: bool,

        /// Final status code to send after playback
        status_code: StatusCode,

        /// Optional reason phrase
        reason: Option<String>,
    },

    /// Transfer to another destination
    Transfer(TransferEndpoint),
}

impl Default for FailureAction {
    fn default() -> Self {
        Self::Hangup {
            code: None,
            reason: None,
        }
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
    pub ice_servers: Option<Vec<IceServer>>,
    pub enable_latching: bool,
}

impl MediaConfig {
    pub fn new() -> Self {
        Self {
            proxy_mode: MediaProxyMode::Auto,
            external_ip: None,
            rtp_start_port: None,
            rtp_end_port: None,
            ice_servers: None,
            enable_latching: false,
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

    pub fn with_ice_servers(mut self, servers: Option<Vec<IceServer>>) -> Self {
        self.ice_servers = servers;
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

#[derive(Clone, Copy, Debug)]
pub enum DialDirection {
    Outbound, // 1. Outbound call initiated by us, usually to a PSTN gateway or another relay server
    Inbound,  // 2. Inbound call received by us, usually from a PSTN gateway
    Internal, // 3. User to user call, both sides are internal
}

impl ToString for DialDirection {
    fn to_string(&self) -> String {
        match self {
            DialDirection::Outbound => "outbound".to_string(),
            DialDirection::Inbound => "inbound".to_string(),
            DialDirection::Internal => "internal".to_string(),
        }
    }
}

pub struct Dialplan {
    pub direction: DialDirection,
    pub call_id: Option<String>,
    pub session_id: Option<String>,
    pub caller_contact: Option<rsip::typed::Contact>,
    pub caller_display_name: Option<String>,
    pub caller: Option<rsip::Uri>,
    pub flow: DialplanFlow,
    pub max_ring_time: u32,
    pub original: Arc<rsip::Request>,
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
    /// Enable SIP flow recording (SIP message logging)
    pub enable_sipflow: bool,

    pub call_forwarding: Option<CallForwardingConfig>,

    pub route_invite: Option<Box<dyn RouteInvite>>,
    pub with_original_headers: bool,
    pub extensions: http::Extensions,
    pub allow_codecs: Vec<CodecType>,
}

impl std::fmt::Debug for Dialplan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialplan")
            .field("direction", &self.direction)
            .field("session_id", &self.session_id)
            .field("caller", &self.caller)
            .field("flow", &self.flow)
            .field("max_ring_time", &self.max_ring_time)
            .field("recording", &self.recording)
            .field("media", &self.media)
            .field("call_timeout", &self.call_timeout)
            .field("enable_sipflow", &self.enable_sipflow)
            .finish()
    }
}

impl Dialplan {
    pub fn is_empty(&self) -> bool {
        self.flow.is_empty()
    }

    pub fn all_webrtc_target(&self) -> bool {
        self.flow.all_webrtc_target()
    }
    /// Create a new dialplan with basic configuration
    pub fn new(session_id: String, original: rsip::Request, direction: DialDirection) -> Self {
        Self {
            direction,
            session_id: Some(session_id),
            call_id: None,
            original: Arc::new(original),
            caller_display_name: None,
            caller: None,
            caller_contact: None,
            flow: DialplanFlow::Targets(DialStrategy::Sequential(vec![])),
            max_ring_time: 60,
            recording: CallRecordingConfig::default(),
            ringback: RingbackConfig::default(),
            media: MediaConfig::default(),
            max_call_duration: Some(Duration::from_secs(3600)), // 1 hour
            call_timeout: Duration::from_secs(60),              // 60 seconds
            failure_action: FailureAction::default(),
            enable_sipflow: true, // Enable SIP flow recording by default
            call_forwarding: None,
            route_invite: None,
            with_original_headers: true,
            extensions: http::Extensions::new(),
            allow_codecs: vec![
                CodecType::G729,
                CodecType::G722,
                CodecType::PCMU,
                CodecType::PCMA,
                #[cfg(feature = "opus")]
                CodecType::Opus,
                CodecType::TelephoneEvent,
            ],
        }
    }

    /// Set the caller URI
    pub fn with_caller(mut self, caller: rsip::Uri) -> Self {
        self.caller = Some(caller);
        self
    }
    pub fn with_targets(mut self, targets: DialStrategy) -> Self {
        self.set_terminal_flow(DialplanFlow::Targets(targets));
        self
    }

    pub fn with_recording(mut self, recording: CallRecordingConfig) -> Self {
        self.recording = recording;
        self
    }

    pub fn with_ringback(mut self, ringback: RingbackConfig) -> Self {
        self.ringback = ringback;
        self
    }

    pub fn with_media(mut self, media: MediaConfig) -> Self {
        self.media = media;
        self
    }

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

    pub fn with_call_forwarding(mut self, config: Option<CallForwardingConfig>) -> Self {
        self.call_forwarding = config;
        self
    }

    pub fn with_queue(mut self, queue: QueuePlan) -> Self {
        let current = std::mem::replace(
            &mut self.flow,
            DialplanFlow::Targets(DialStrategy::Sequential(vec![])),
        );
        self.flow = DialplanFlow::Queue {
            plan: queue,
            next: Box::new(current),
        };
        self
    }

    pub fn with_caller_contact(mut self, contact: rsip::typed::Contact) -> Self {
        self.caller_contact = Some(contact);
        self
    }

    pub fn with_extension<T: Clone + Send + Sync + 'static>(mut self, val: T) -> Self {
        self.extensions.insert(val);
        self
    }

    /// Get all target locations regardless of strategy
    pub fn get_all_targets(&self) -> Option<&Vec<Location>> {
        self.flow.find_targets()
    }

    pub fn first_target(&self) -> Option<&Location> {
        self.get_all_targets().and_then(|targets| targets.first())
    }

    /// Check if using parallel dialing strategy
    pub fn is_parallel_strategy(&self) -> bool {
        self.flow.is_parallel()
    }

    pub fn has_queue(&self) -> bool {
        self.flow.has_queue()
    }

    pub fn has_queue_hold_audio(&self) -> bool {
        self.flow.has_queue_hold_audio()
    }

    /// Check if recording is enabled
    pub fn is_recording_enabled(&self) -> bool {
        self.recording.enabled
    }

    fn set_terminal_flow(&mut self, new_terminal: DialplanFlow) {
        let current = std::mem::replace(
            &mut self.flow,
            DialplanFlow::Targets(DialStrategy::Sequential(vec![])),
        );
        self.flow = DialplanFlow::replace_terminal(current, new_terminal);
    }

    pub fn should_forward_header(header: &rsip::Header) -> bool {
        use rsip::Header;

        match header {
            Header::Via(_)
            | Header::Contact(_)
            | Header::From(_)
            | Header::To(_)
            | Header::CallId(_)
            | Header::CSeq(_)
            | Header::MaxForwards(_)
            | Header::ContentLength(_)
            | Header::ContentType(_)
            | Header::Authorization(_)
            | Header::ProxyAuthorization(_)
            | Header::ProxyAuthenticate(_)
            | Header::WwwAuthenticate(_)
            | Header::Route(_)
            | Header::UserAgent(_)
            | Header::Allow(_)
            | Header::Supported(_)
            | Header::RecordRoute(_) => false,
            Header::Other(name, _) => {
                let lower = name.to_ascii_lowercase();
                !matches!(
                    lower.as_str(),
                    "via"
                        | "from"
                        | "to"
                        | "contact"
                        | "call-id"
                        | "cseq"
                        | "max-forwards"
                        | "content-length"
                        | "content-type"
                        | "route"
                        | "record-route"
                        | "authorization"
                        | "proxy-authorization"
                        | "proxy-authenticate"
                        | "www-authenticate"
                        | "user-agent"
                        | "allow"
                        | "supported"
                )
            }
            _ => true,
        }
    }

    pub fn build_invite_headers(&self, target: &Location) -> Option<Vec<rsip::Header>> {
        let mut headers = target.headers.clone().unwrap_or_default();
        if self.with_original_headers {
            for header in self.original.headers.iter() {
                if !Self::should_forward_header(header) {
                    continue;
                }
                headers.push(header.clone());
            }
        }

        if headers.is_empty() {
            None
        } else {
            Some(headers)
        }
    }
}

#[async_trait::async_trait]
pub trait RouteInvite: Sync + Send {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsip::Request,
        direction: &DialDirection,
        cookie: &TransactionCookie,
    ) -> Result<RouteResult>;

    async fn preview_route(
        &self,
        option: InviteOption,
        origin: &rsip::Request,
        direction: &DialDirection,
        cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        self.route_invite(option, origin, direction, cookie).await
    }
}

/// Routing state for managing stateful load balancing
#[derive(Debug)]
pub struct RoutingState {
    /// Round-robin counters for each destination group
    round_robin_counters: Arc<Mutex<HashMap<String, usize>>>,
    pub policy_guard: Option<Arc<crate::call::policy::PolicyGuard>>,
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
            policy_guard: None,
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
