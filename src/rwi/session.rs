use crate::rwi::auth::RwiIdentity;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RwiSession {
    pub id: String,
    pub identity: RwiIdentity,
    pub subscribed_contexts: HashSet<String>,
    pub owned_calls: HashMap<String, CallOwnership>,
    pub supervisor_targets: HashMap<String, SupervisorMode>,
    pub command_tx: mpsc::UnboundedSender<RwiCommandMessage>,
    pub created_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct CallOwnership {
    pub call_id: String,
    pub mode: OwnershipMode,
    pub created_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OwnershipMode {
    Control,
    Listen,
    Whisper,
    Barge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorMode {
    Listen,
    Whisper,
    Barge,
}

#[derive(Debug, Clone)]
pub struct RwiCommandMessage {
    pub id: String,
    pub call_id: Option<String>,
    pub command: RwiCommandPayload,
}

#[derive(Debug, Clone)]
pub enum RwiCommandPayload {
    Subscribe {
        contexts: Vec<String>,
    },
    Unsubscribe {
        contexts: Vec<String>,
    },
    ListCalls,
    AttachCall {
        call_id: String,
        mode: OwnershipMode,
    },
    DetachCall {
        call_id: String,
    },
    Originate(OriginateRequest),
    Answer {
        call_id: String,
    },
    Reject {
        call_id: String,
        reason: Option<String>,
    },
    Ring {
        call_id: String,
    },
    Hangup {
        call_id: String,
        reason: Option<String>,
        code: Option<u16>,
    },
    Bridge {
        leg_a: String,
        leg_b: String,
    },
    Unbridge {
        call_id: String,
    },
    Transfer {
        call_id: String,
        target: String,
    },
    /// Attended transfer: agent talks to target first, then completes transfer.
    /// Steps: 1) call.call_id is placed on hold, 2) new leg created to target,
    /// 3) when target answers, agent can complete or cancel transfer.
    TransferAttended {
        call_id: String,
        target: String,
        /// Optional timeout for the consultation call
        timeout_secs: Option<u32>,
    },
    /// Complete an attended transfer after consultation call is established.
    TransferComplete {
        /// The original call (caller)
        call_id: String,
        /// The consultation call (target)
        consultation_call_id: String,
    },
    /// Cancel an attended transfer, returning to the original call.
    TransferCancel {
        /// The consultation call to hang up
        consultation_call_id: String,
    },
    /// Place a call on hold with optional music.
    CallHold {
        call_id: String,
        music: Option<String>,
    },
    /// Release a call from hold.
    CallUnhold {
        call_id: String,
    },
    SetRingbackSource {
        target_call_id: String,
        source_call_id: String,
    },
    MediaPlay(MediaPlayRequest),
    MediaStop {
        call_id: String,
    },
    MediaStreamStart(MediaStreamRequest),
    MediaStreamStop {
        call_id: String,
    },
    MediaInjectStart(MediaInjectRequest),
    MediaInjectStop {
        call_id: String,
    },
    RecordStart(RecordStartRequest),
    RecordPause {
        call_id: String,
    },
    RecordResume {
        call_id: String,
    },
    RecordStop {
        call_id: String,
    },
    QueueEnqueue(QueueEnqueueRequest),
    QueueDequeue {
        call_id: String,
    },
    QueueHold {
        call_id: String,
    },
    QueueUnhold {
        call_id: String,
    },
    QueueSetPriority {
        call_id: String,
        priority: u32,
    },
    QueueAssignAgent {
        call_id: String,
        agent_id: String,
    },
    QueueRequeue {
        call_id: String,
        queue_id: String,
        priority: Option<u32>,
    },
    SupervisorListen {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SupervisorWhisper {
        supervisor_call_id: String,
        target_call_id: String,
        agent_leg: String,
    },
    SupervisorBarge {
        supervisor_call_id: String,
        target_call_id: String,
        agent_leg: String,
    },
    SupervisorStop {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SipMessage {
        call_id: String,
        content_type: String,
        body: String,
    },
    SipNotify {
        call_id: String,
        event: String,
        content_type: String,
        body: String,
    },
    SipOptionsPing {
        call_id: String,
    },
    ConferenceCreate(ConferenceCreateRequest),
    ConferenceAdd {
        conf_id: String,
        call_id: String,
    },
    ConferenceRemove {
        conf_id: String,
        call_id: String,
    },
    ConferenceMute {
        conf_id: String,
        call_id: String,
    },
    ConferenceUnmute {
        conf_id: String,
        call_id: String,
    },
    ConferenceDestroy {
        conf_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct OriginateRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub destination: String,
    pub caller_id: Option<String>,
    pub timeout_secs: Option<u32>,
    pub hold_music: Option<MediaSource>,
    pub hold_music_target: Option<String>,
    pub ringback: Option<String>,
    pub ringback_target: Option<String>,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MediaSource {
    #[serde(default)]
    pub source_type: String,
    pub uri: Option<String>,
    pub looped: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaPlayRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub source: MediaSource,
    #[serde(default)]
    pub interrupt_on_dtmf: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaStreamRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default = "default_direction")]
    pub direction: String,
    #[serde(default)]
    pub format: MediaFormat,
}

fn default_direction() -> String {
    "sendrecv".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaFormat {
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u32,
    pub ptime_ms: Option<u32>,
}

impl Default for MediaFormat {
    fn default() -> Self {
        MediaFormat {
            codec: default_codec(),
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            ptime_ms: None,
        }
    }
}

fn default_codec() -> String {
    "PCMU".to_string()
}

fn default_sample_rate() -> u32 {
    8000
}

fn default_channels() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaInjectRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub format: MediaFormat,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordStartRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    pub beep: Option<bool>,
    pub max_duration_secs: Option<u32>,
    #[serde(default)]
    pub storage: RecordStorage,
}

fn default_mode() -> String {
    "mixed".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordStorage {
    #[serde(default = "default_record_backend")]
    pub backend: String,
    #[serde(default)]
    pub path: String,
}

impl Default for RecordStorage {
    fn default() -> Self {
        RecordStorage {
            backend: default_record_backend(),
            path: String::new(),
        }
    }
}

fn default_record_backend() -> String {
    "file".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueueEnqueueRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub queue_id: String,
    pub priority: Option<u32>,
    pub skills: Option<Vec<String>>,
    pub max_wait_secs: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceCreateRequest {
    #[serde(default)]
    pub conf_id: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub max_members: Option<u32>,
    #[serde(default)]
    pub record: bool,
    pub mcu_uri: Option<String>,
}

fn default_backend() -> String {
    "internal".to_string()
}

/// RWI request enum for deserializing client commands
#[derive(Deserialize)]
#[serde(tag = "action", content = "params")]
pub enum RwiRequest {
    // Session commands
    #[serde(alias = "session.subscribe")]
    Subscribe { contexts: Option<Vec<String>> },
    #[serde(alias = "session.unsubscribe")]
    Unsubscribe { contexts: Option<Vec<String>> },
    #[serde(alias = "session.list_calls")]
    ListCalls,
    #[serde(alias = "session.attach_call")]
    AttachCall {
        call_id: Option<String>,
        mode: Option<String>,
    },
    #[serde(alias = "session.detach_call")]
    DetachCall { call_id: Option<String> },

    // Call commands
    #[serde(alias = "call.originate")]
    Originate(OriginateRequest),
    #[serde(alias = "call.answer")]
    Answer { call_id: Option<String> },
    #[serde(alias = "call.reject")]
    Reject {
        call_id: Option<String>,
        reason: Option<String>,
    },
    #[serde(alias = "call.ring")]
    Ring { call_id: Option<String> },
    #[serde(alias = "call.hangup")]
    Hangup {
        call_id: Option<String>,
        reason: Option<String>,
        code: Option<u16>,
    },
    #[serde(alias = "call.bridge")]
    Bridge {
        leg_a: Option<String>,
        leg_b: Option<String>,
    },
    #[serde(alias = "call.unbridge")]
    Unbridge { call_id: Option<String> },
    #[serde(alias = "call.transfer")]
    Transfer {
        call_id: Option<String>,
        target: Option<String>,
    },
    #[serde(alias = "call.transfer.attended")]
    TransferAttended {
        call_id: Option<String>,
        target: Option<String>,
        timeout_secs: Option<u32>,
    },
    #[serde(alias = "call.transfer.complete")]
    TransferComplete {
        call_id: Option<String>,
        consultation_call_id: Option<String>,
    },
    #[serde(alias = "call.transfer.cancel")]
    TransferCancel {
        consultation_call_id: Option<String>,
    },
    #[serde(alias = "call.hold")]
    CallHold {
        call_id: Option<String>,
        music: Option<String>,
    },
    #[serde(alias = "call.unhold")]
    CallUnhold { call_id: Option<String> },
    #[serde(alias = "call.set_ringback_source")]
    SetRingbackSource {
        target_call_id: Option<String>,
        source_call_id: Option<String>,
    },

    // Media commands
    #[serde(alias = "media.play")]
    MediaPlay(MediaPlayRequest),
    #[serde(alias = "media.stop")]
    MediaStop { call_id: Option<String> },
    #[serde(alias = "media.stream_start")]
    MediaStreamStart(MediaStreamRequest),
    #[serde(alias = "media.stream_stop")]
    MediaStreamStop { call_id: Option<String> },
    #[serde(alias = "media.inject_start")]
    MediaInjectStart(MediaInjectRequest),
    #[serde(alias = "media.inject_stop")]
    MediaInjectStop { call_id: Option<String> },

    // Record commands
    #[serde(alias = "record.start")]
    RecordStart(RecordStartRequest),
    #[serde(alias = "record.pause")]
    RecordPause { call_id: Option<String> },
    #[serde(alias = "record.resume")]
    RecordResume { call_id: Option<String> },
    #[serde(alias = "record.stop")]
    RecordStop { call_id: Option<String> },

    // Queue commands
    #[serde(alias = "queue.enqueue")]
    QueueEnqueue(QueueEnqueueRequest),
    #[serde(alias = "queue.dequeue")]
    QueueDequeue { call_id: Option<String> },
    #[serde(alias = "queue.hold")]
    QueueHold { call_id: Option<String> },
    #[serde(alias = "queue.unhold")]
    QueueUnhold { call_id: Option<String> },
    #[serde(alias = "queue.set_priority")]
    QueueSetPriority {
        call_id: Option<String>,
        priority: Option<u32>,
    },
    #[serde(alias = "queue.assign_agent")]
    QueueAssignAgent {
        call_id: Option<String>,
        agent_id: Option<String>,
    },
    #[serde(alias = "queue.requeue")]
    QueueRequeue {
        call_id: Option<String>,
        queue_id: Option<String>,
        priority: Option<u32>,
    },

    // Supervisor commands
    #[serde(alias = "supervisor.listen")]
    SupervisorListen {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
    },
    #[serde(alias = "supervisor.whisper")]
    SupervisorWhisper {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
        agent_leg: Option<String>,
    },
    #[serde(alias = "supervisor.barge")]
    SupervisorBarge {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
        agent_leg: Option<String>,
    },
    #[serde(alias = "supervisor.stop")]
    SupervisorStop {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
    },

    // SIP commands
    #[serde(alias = "sip.message")]
    SipMessage {
        call_id: Option<String>,
        content_type: Option<String>,
        body: Option<String>,
    },
    #[serde(alias = "sip.notify")]
    SipNotify {
        call_id: Option<String>,
        event: Option<String>,
        content_type: Option<String>,
        body: Option<String>,
    },
    #[serde(alias = "sip.options_ping")]
    SipOptionsPing { call_id: Option<String> },

    // Conference commands
    #[serde(alias = "conference.create")]
    ConferenceCreate(ConferenceCreateRequest),
    #[serde(alias = "conference.add")]
    ConferenceAdd {
        conf_id: Option<String>,
        call_id: Option<String>,
    },
    #[serde(alias = "conference.remove")]
    ConferenceRemove {
        conf_id: Option<String>,
        call_id: Option<String>,
    },
    #[serde(alias = "conference.mute")]
    ConferenceMute {
        conf_id: Option<String>,
        call_id: Option<String>,
    },
    #[serde(alias = "conference.unmute")]
    ConferenceUnmute {
        conf_id: Option<String>,
        call_id: Option<String>,
    },
    #[serde(alias = "conference.destroy")]
    ConferenceDestroy { conf_id: Option<String> },
}

impl From<RwiRequest> for RwiCommandPayload {
    fn from(req: RwiRequest) -> Self {
        match req {
            RwiRequest::Subscribe { contexts } => RwiCommandPayload::Subscribe {
                contexts: contexts.unwrap_or_default(),
            },
            RwiRequest::Unsubscribe { contexts } => RwiCommandPayload::Unsubscribe {
                contexts: contexts.unwrap_or_default(),
            },
            RwiRequest::ListCalls => RwiCommandPayload::ListCalls,
            RwiRequest::AttachCall { call_id, mode } => RwiCommandPayload::AttachCall {
                call_id: call_id.unwrap_or_default(),
                mode: match mode.as_deref() {
                    Some("listen") => OwnershipMode::Listen,
                    Some("whisper") => OwnershipMode::Whisper,
                    Some("barge") => OwnershipMode::Barge,
                    _ => OwnershipMode::Control,
                },
            },
            RwiRequest::DetachCall { call_id } => RwiCommandPayload::DetachCall {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::Originate(mut r) => {
                // Set defaults
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                if r.destination.is_empty() {
                    r.destination = String::new();
                }
                r.extra_headers = HashMap::new();
                r.hold_music = None;
                RwiCommandPayload::Originate(r)
            }
            RwiRequest::Answer { call_id } => RwiCommandPayload::Answer {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::Reject { call_id, reason } => RwiCommandPayload::Reject {
                call_id: call_id.unwrap_or_default(),
                reason,
            },
            RwiRequest::Ring { call_id } => RwiCommandPayload::Ring {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::Hangup {
                call_id,
                reason,
                code,
            } => RwiCommandPayload::Hangup {
                call_id: call_id.unwrap_or_default(),
                reason,
                code,
            },
            RwiRequest::Bridge { leg_a, leg_b } => RwiCommandPayload::Bridge {
                leg_a: leg_a.unwrap_or_default(),
                leg_b: leg_b.unwrap_or_default(),
            },
            RwiRequest::Unbridge { call_id } => RwiCommandPayload::Unbridge {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::Transfer { call_id, target } => RwiCommandPayload::Transfer {
                call_id: call_id.unwrap_or_default(),
                target: target.unwrap_or_default(),
            },
            RwiRequest::TransferAttended {
                call_id,
                target,
                timeout_secs,
            } => RwiCommandPayload::TransferAttended {
                call_id: call_id.unwrap_or_default(),
                target: target.unwrap_or_default(),
                timeout_secs,
            },
            RwiRequest::TransferComplete {
                call_id,
                consultation_call_id,
            } => RwiCommandPayload::TransferComplete {
                call_id: call_id.unwrap_or_default(),
                consultation_call_id: consultation_call_id.unwrap_or_default(),
            },
            RwiRequest::TransferCancel {
                consultation_call_id,
            } => RwiCommandPayload::TransferCancel {
                consultation_call_id: consultation_call_id.unwrap_or_default(),
            },
            RwiRequest::CallHold { call_id, music } => RwiCommandPayload::CallHold {
                call_id: call_id.unwrap_or_default(),
                music,
            },
            RwiRequest::CallUnhold { call_id } => RwiCommandPayload::CallUnhold {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::SetRingbackSource {
                target_call_id,
                source_call_id,
            } => RwiCommandPayload::SetRingbackSource {
                target_call_id: target_call_id.unwrap_or_default(),
                source_call_id: source_call_id.unwrap_or_default(),
            },
            RwiRequest::MediaPlay(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaPlay(r)
            }
            RwiRequest::MediaStop { call_id } => RwiCommandPayload::MediaStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::MediaStreamStart(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaStreamStart(r)
            }
            RwiRequest::MediaStreamStop { call_id } => RwiCommandPayload::MediaStreamStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::MediaInjectStart(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaInjectStart(r)
            }
            RwiRequest::MediaInjectStop { call_id } => RwiCommandPayload::MediaInjectStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::RecordStart(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::RecordStart(r)
            }
            RwiRequest::RecordPause { call_id } => RwiCommandPayload::RecordPause {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::RecordResume { call_id } => RwiCommandPayload::RecordResume {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::RecordStop { call_id } => RwiCommandPayload::RecordStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::QueueEnqueue(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::QueueEnqueue(r)
            }
            RwiRequest::QueueDequeue { call_id } => RwiCommandPayload::QueueDequeue {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::QueueHold { call_id } => RwiCommandPayload::QueueHold {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::QueueUnhold { call_id } => RwiCommandPayload::QueueUnhold {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::QueueSetPriority { call_id, priority } => {
                RwiCommandPayload::QueueSetPriority {
                    call_id: call_id.unwrap_or_default(),
                    priority: priority.unwrap_or(0),
                }
            }
            RwiRequest::QueueAssignAgent { call_id, agent_id } => {
                RwiCommandPayload::QueueAssignAgent {
                    call_id: call_id.unwrap_or_default(),
                    agent_id: agent_id.unwrap_or_default(),
                }
            }
            RwiRequest::QueueRequeue {
                call_id,
                queue_id,
                priority,
            } => RwiCommandPayload::QueueRequeue {
                call_id: call_id.unwrap_or_default(),
                queue_id: queue_id.unwrap_or_default(),
                priority,
            },
            RwiRequest::SupervisorListen {
                supervisor_call_id,
                target_call_id,
            } => RwiCommandPayload::SupervisorListen {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
            },
            RwiRequest::SupervisorWhisper {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
                agent_leg: agent_leg.unwrap_or_default(),
            },
            RwiRequest::SupervisorBarge {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => RwiCommandPayload::SupervisorBarge {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
                agent_leg: agent_leg.unwrap_or_default(),
            },
            RwiRequest::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => RwiCommandPayload::SupervisorStop {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
            },
            RwiRequest::SipMessage {
                call_id,
                content_type,
                body,
            } => RwiCommandPayload::SipMessage {
                call_id: call_id.unwrap_or_default(),
                content_type: content_type.unwrap_or_else(|| "text/plain".to_string()),
                body: body.unwrap_or_default(),
            },
            RwiRequest::SipNotify {
                call_id,
                event,
                content_type,
                body,
            } => RwiCommandPayload::SipNotify {
                call_id: call_id.unwrap_or_default(),
                event: event.unwrap_or_default(),
                content_type: content_type.unwrap_or_else(|| "application/json".to_string()),
                body: body.unwrap_or_default(),
            },
            RwiRequest::SipOptionsPing { call_id } => RwiCommandPayload::SipOptionsPing {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::ConferenceCreate(mut r) => {
                if r.conf_id.is_empty() {
                    r.conf_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::ConferenceCreate(r)
            }
            RwiRequest::ConferenceAdd { conf_id, call_id } => RwiCommandPayload::ConferenceAdd {
                conf_id: conf_id.unwrap_or_default(),
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::ConferenceRemove { conf_id, call_id } => {
                RwiCommandPayload::ConferenceRemove {
                    conf_id: conf_id.unwrap_or_default(),
                    call_id: call_id.unwrap_or_default(),
                }
            }
            RwiRequest::ConferenceMute { conf_id, call_id } => RwiCommandPayload::ConferenceMute {
                conf_id: conf_id.unwrap_or_default(),
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequest::ConferenceUnmute { conf_id, call_id } => {
                RwiCommandPayload::ConferenceUnmute {
                    conf_id: conf_id.unwrap_or_default(),
                    call_id: call_id.unwrap_or_default(),
                }
            }
            RwiRequest::ConferenceDestroy { conf_id } => RwiCommandPayload::ConferenceDestroy {
                conf_id: conf_id.unwrap_or_default(),
            },
        }
    }
}

impl RwiSession {
    pub fn new(
        identity: RwiIdentity,
        command_tx: mpsc::UnboundedSender<RwiCommandMessage>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            identity,
            subscribed_contexts: HashSet::new(),
            owned_calls: HashMap::new(),
            supervisor_targets: HashMap::new(),
            command_tx,
            created_at: std::time::Instant::now(),
        }
    }

    pub fn subscribe(&mut self, contexts: Vec<String>) {
        for ctx in contexts {
            self.subscribed_contexts.insert(ctx);
        }
    }

    pub fn unsubscribe(&mut self, contexts: &[String]) {
        for ctx in contexts {
            self.subscribed_contexts.remove(ctx);
        }
    }

    pub fn owns_call(&self, call_id: &str) -> bool {
        self.owned_calls.contains_key(call_id)
    }

    pub fn owns_call_in_mode(&self, call_id: &str, mode: &OwnershipMode) -> bool {
        self.owned_calls
            .get(call_id)
            .map(|o| &o.mode == mode)
            .unwrap_or(false)
    }

    pub fn claim_call(&mut self, call_id: String, mode: OwnershipMode) -> bool {
        if self.owned_calls.contains_key(&call_id) {
            return false;
        }
        let owned = CallOwnership {
            call_id: call_id.clone(),
            mode,
            created_at: std::time::Instant::now(),
        };
        self.owned_calls.insert(call_id, owned);
        true
    }

    pub fn release_call(&mut self, call_id: &str) -> bool {
        self.owned_calls.remove(call_id).is_some()
    }

    pub fn add_supervisor_target(&mut self, target_call_id: String, mode: SupervisorMode) {
        self.supervisor_targets.insert(target_call_id, mode);
    }

    pub fn remove_supervisor_target(&mut self, target_call_id: &str) -> bool {
        self.supervisor_targets.remove(target_call_id).is_some()
    }

    pub fn is_supervisor_of(&self, call_id: &str) -> bool {
        self.supervisor_targets.contains_key(call_id)
    }

    pub fn get_supervisor_mode(&self, call_id: &str) -> Option<&SupervisorMode> {
        self.supervisor_targets.get(call_id)
    }

    pub fn list_owned_calls(&self) -> Vec<String> {
        self.owned_calls.keys().cloned().collect()
    }

    pub fn can_control_call(&self, call_id: &str) -> bool {
        self.owned_calls
            .get(call_id)
            .map(|o| o.mode == OwnershipMode::Control)
            .unwrap_or(false)
    }

    pub fn can_listen_to_call(&self, call_id: &str) -> bool {
        self.owns_call(call_id) || self.supervisor_targets.contains_key(call_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rwi::auth::RwiIdentity;
    use tokio::sync::mpsc;

    fn create_test_identity() -> RwiIdentity {
        RwiIdentity {
            token: "test-token".to_string(),
            scopes: vec!["call.control".to_string()],
        }
    }

    fn create_test_session() -> (RwiSession, mpsc::UnboundedReceiver<RwiCommandMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let identity = create_test_identity();
        let session = RwiSession::new(identity, tx);
        (session, rx)
    }

    #[test]
    fn test_session_creation() {
        let identity = create_test_identity();
        let (tx, _rx) = mpsc::unbounded_channel();
        let session = RwiSession::new(identity.clone(), tx);

        assert!(!session.id.is_empty());
        assert_eq!(session.identity.token, "test-token");
        assert!(session.subscribed_contexts.is_empty());
        assert!(session.owned_calls.is_empty());
    }

    #[test]
    fn test_subscribe() {
        let (mut session, _rx) = create_test_session();
        session.subscribe(vec!["context1".to_string(), "context2".to_string()]);

        assert!(session.subscribed_contexts.contains("context1"));
        assert!(session.subscribed_contexts.contains("context2"));
        assert_eq!(session.subscribed_contexts.len(), 2);
    }

    #[test]
    fn test_unsubscribe() {
        let (mut session, _rx) = create_test_session();
        session.subscribe(vec!["context1".to_string(), "context2".to_string()]);
        session.unsubscribe(&["context1".to_string()]);

        assert!(!session.subscribed_contexts.contains("context1"));
        assert!(session.subscribed_contexts.contains("context2"));
    }

    #[test]
    fn test_claim_call() {
        let (mut session, _rx) = create_test_session();

        let result = session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(result);
        assert!(session.owns_call("call-001"));

        let result = session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(!result);
    }

    #[test]
    fn test_claim_call_in_mode() {
        let (mut session, _rx) = create_test_session();

        session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(session.owns_call_in_mode("call-001", &OwnershipMode::Control));
        assert!(!session.owns_call_in_mode("call-001", &OwnershipMode::Listen));

        session.claim_call("call-002".to_string(), OwnershipMode::Listen);
        assert!(session.owns_call_in_mode("call-002", &OwnershipMode::Listen));
    }

    #[test]
    fn test_release_call() {
        let (mut session, _rx) = create_test_session();

        session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(session.owns_call("call-001"));

        let result = session.release_call("call-001");
        assert!(result);
        assert!(!session.owns_call("call-001"));

        let result = session.release_call("nonexistent");
        assert!(!result);
    }

    #[test]
    fn test_list_owned_calls() {
        let (mut session, _rx) = create_test_session();

        session.claim_call("call-001".to_string(), OwnershipMode::Control);
        session.claim_call("call-002".to_string(), OwnershipMode::Listen);

        let calls = session.list_owned_calls();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&"call-001".to_string()));
        assert!(calls.contains(&"call-002".to_string()));
    }

    #[test]
    fn test_can_control_call() {
        let (mut session, _rx) = create_test_session();

        session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(session.can_control_call("call-001"));

        session.claim_call("call-002".to_string(), OwnershipMode::Listen);
        assert!(!session.can_control_call("call-002"));
        assert!(!session.can_control_call("nonexistent"));
    }

    #[test]
    fn test_supervisor_targets() {
        let (mut session, _rx) = create_test_session();

        session.add_supervisor_target("call-001".to_string(), SupervisorMode::Listen);
        assert!(session.is_supervisor_of("call-001"));

        let mode = session.get_supervisor_mode("call-001");
        assert!(mode.is_some());
        assert!(matches!(mode.unwrap(), SupervisorMode::Listen));

        let removed = session.remove_supervisor_target("call-001");
        assert!(removed);
        assert!(!session.is_supervisor_of("call-001"));
    }

    #[test]
    fn test_can_listen_to_call() {
        let (mut session, _rx) = create_test_session();

        session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(session.can_listen_to_call("call-001"));

        session.add_supervisor_target("call-002".to_string(), SupervisorMode::Barge);
        assert!(session.can_listen_to_call("call-002"));

        assert!(!session.can_listen_to_call("nonexistent"));
    }

    #[test]
    fn test_ownership_mode_variants() {
        let mode1 = OwnershipMode::Control;
        let mode2 = OwnershipMode::Control;
        assert_eq!(mode1, mode2);

        let mode3 = OwnershipMode::Listen;
        assert_ne!(mode1, mode3);
    }

    #[test]
    fn test_supervisor_mode_variants() {
        let mode1 = SupervisorMode::Listen;
        let mode2 = SupervisorMode::Listen;
        assert_eq!(mode1, mode2);

        let mode3 = SupervisorMode::Whisper;
        assert_ne!(mode1, mode3);
    }

    #[test]
    fn test_originate_request() {
        let request = OriginateRequest {
            call_id: "new-call".to_string(),
            destination: "sip:test@local".to_string(),
            caller_id: Some("1001".to_string()),
            timeout_secs: Some(30),
            hold_music: Some(MediaSource {
                source_type: "file".to_string(),
                uri: Some("hold.wav".to_string()),
                looped: Some(true),
            }),
            hold_music_target: Some("call-001".to_string()),
            ringback: Some("local".to_string()),
            ringback_target: None,
            extra_headers: {
                let mut h = HashMap::new();
                h.insert("X-Custom".to_string(), "value".to_string());
                h
            },
        };

        assert_eq!(request.call_id, "new-call");
        assert_eq!(request.destination, "sip:test@local");
        assert_eq!(request.caller_id, Some("1001".to_string()));
        assert!(request.hold_music.is_some());
        assert_eq!(
            request.extra_headers.get("X-Custom"),
            Some(&"value".to_string())
        );
    }

    #[test]
    fn test_media_play_request() {
        let request = MediaPlayRequest {
            call_id: "call-001".to_string(),
            source: MediaSource {
                source_type: "file".to_string(),
                uri: Some("welcome.wav".to_string()),
                looped: None,
            },
            interrupt_on_dtmf: true,
        };

        assert_eq!(request.call_id, "call-001");
        assert!(request.interrupt_on_dtmf);
    }

    #[test]
    fn test_media_format() {
        let format = MediaFormat {
            codec: "PCMU".to_string(),
            sample_rate: 8000,
            channels: 1,
            ptime_ms: Some(20),
        };

        assert_eq!(format.codec, "PCMU");
        assert_eq!(format.sample_rate, 8000);
        assert_eq!(format.ptime_ms, Some(20));
    }

    #[test]
    fn test_record_start_request() {
        let request = RecordStartRequest {
            call_id: "call-001".to_string(),
            mode: "mixed".to_string(),
            beep: Some(true),
            max_duration_secs: Some(3600),
            storage: RecordStorage {
                backend: "file".to_string(),
                path: "/var/recordings".to_string(),
            },
        };

        assert_eq!(request.call_id, "call-001");
        assert_eq!(request.beep, Some(true));
        assert_eq!(request.storage.backend, "file");
    }

    #[test]
    fn test_queue_enqueue_request() {
        let request = QueueEnqueueRequest {
            call_id: "call-001".to_string(),
            queue_id: "support".to_string(),
            priority: Some(5),
            skills: Some(vec!["en".to_string(), "tech".to_string()]),
            max_wait_secs: Some(120),
        };

        assert_eq!(request.queue_id, "support");
        assert_eq!(request.priority, Some(5));
        assert_eq!(
            request.skills,
            Some(vec!["en".to_string(), "tech".to_string()])
        );
    }
}
