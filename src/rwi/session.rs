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
    pub action_id: String,
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
    TransferReplace {
        call_id: String,
        target: String,
    },
    TransferAttended {
        call_id: String,
        target: String,
        timeout_secs: Option<u32>,
    },
    TransferComplete {
        call_id: String,
        consultation_call_id: String,
    },
    TransferCancel {
        consultation_call_id: String,
    },
    CallHold {
        call_id: String,
        music: Option<String>,
    },
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
    SupervisorTakeover {
        supervisor_call_id: String,
        target_call_id: String,
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
    LegAdd {
        call_id: String,
        target: String,
        leg_id: Option<String>,
    },
    LegRemove {
        call_id: String,
        leg_id: String,
    },
    AppStart {
        call_id: String,
        app_name: String,
        params: Option<serde_json::Value>,
    },
    AppStop {
        call_id: String,
        reason: Option<String>,
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
    ConferenceMerge {
        conf_id: String,
        call_id: String,
        consultation_call_id: String,
    },
    ConferenceSeatReplace {
        conf_id: String,
        old_call_id: String,
        new_call_id: String,
    },
    ParallelOriginate(ParallelOriginateRequest),
    SessionResume {
        last_sequence: Option<u64>,
    },
    CallResume {
        call_id: String,
        last_sequence: Option<u64>,
    },
    // CC addon commands
    AgentRegister {
        agent_id: String,
        tenant_id: String,
        skills: Vec<String>,
        max_concurrency: u32,
    },
    AgentUnregister {
        agent_id: String,
    },
    AgentStatusUpdate {
        agent_id: String,
        status: String,
        call_id: Option<String>,
    },
    AgentStats {
        agent_id: Option<String>,
    },
    QueueStats {
        queue_id: Option<String>,
    },
    ConsultInitiate {
        call_id: String,
        target: String,
    },
    ConsultMerge {
        call_id: String,
        consultation_call_id: String,
    },
    ConsultComplete {
        call_id: String,
        consultation_call_id: String,
    },
    ConsultCancel {
        consultation_call_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParallelOriginateRequest {
    pub operation_id: String,
    pub targets: Vec<OriginateTarget>,
    pub caller_id: Option<String>,
    pub timeout_secs: Option<u32>,
    pub hold_music: Option<MediaSource>,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OriginateTarget {
    pub call_id: String,
    pub destination: String,
    pub timeout_secs: Option<u32>,
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
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub path: String,
}

impl Default for RecordStorage {
    fn default() -> Self {
        Self {
            backend: "file".to_string(),
            path: String::new(),
        }
    }
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
    #[serde(default, alias = "conference_id")]
    pub conf_id: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub max_members: Option<u32>,
    #[serde(default)]
    pub record: bool,
    pub mcu_uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceAddRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceRemoveRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceMuteRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceUnmuteRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceDestroyRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceMergeRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
    pub call_id: Option<String>,
    pub consultation_call_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceSeatReplaceRequest {
    #[serde(alias = "conference_id")]
    pub conf_id: Option<String>,
    pub old_call_id: Option<String>,
    pub new_call_id: Option<String>,
}

fn default_backend() -> String {
    "internal".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct RwiRequest {
    pub action_id: String,
    #[serde(flatten)]
    pub payload: RwiRequestPayload,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", content = "params")]
pub enum RwiRequestPayload {
    #[serde(rename = "session.subscribe", alias = "Subscribe")]
    Subscribe { contexts: Option<Vec<String>> },
    #[serde(rename = "session.unsubscribe", alias = "Unsubscribe")]
    Unsubscribe { contexts: Option<Vec<String>> },
    #[serde(rename = "session.list_calls", alias = "ListCalls")]
    ListCalls,
    #[serde(rename = "session.attach_call")]
    AttachCall {
        call_id: Option<String>,
        mode: Option<String>,
    },
    #[serde(rename = "session.detach_call")]
    DetachCall { call_id: Option<String> },
    #[serde(rename = "call.originate", alias = "Originate")]
    Originate(OriginateRequest),
    #[serde(rename = "call.answer", alias = "Answer")]
    Answer { call_id: Option<String> },
    #[serde(rename = "call.reject")]
    Reject {
        call_id: Option<String>,
        reason: Option<String>,
    },
    #[serde(rename = "call.ring")]
    Ring { call_id: Option<String> },
    #[serde(rename = "call.hangup")]
    Hangup {
        call_id: Option<String>,
        reason: Option<String>,
        code: Option<u16>,
    },
    #[serde(rename = "call.bridge")]
    Bridge {
        leg_a: Option<String>,
        leg_b: Option<String>,
    },
    #[serde(rename = "call.unbridge")]
    Unbridge { call_id: Option<String> },
    #[serde(rename = "call.transfer")]
    Transfer {
        call_id: Option<String>,
        target: Option<String>,
    },
    #[serde(rename = "call.transfer.replace")]
    TransferReplace {
        call_id: Option<String>,
        target: Option<String>,
    },
    #[serde(rename = "call.transfer.attended")]
    TransferAttended {
        call_id: Option<String>,
        target: Option<String>,
        timeout_secs: Option<u32>,
    },
    #[serde(rename = "call.transfer.complete")]
    TransferComplete {
        call_id: Option<String>,
        consultation_call_id: Option<String>,
    },
    #[serde(rename = "call.transfer.cancel")]
    TransferCancel {
        consultation_call_id: Option<String>,
    },
    #[serde(rename = "call.hold")]
    CallHold {
        call_id: Option<String>,
        music: Option<String>,
    },
    #[serde(rename = "call.unhold")]
    CallUnhold { call_id: Option<String> },
    #[serde(rename = "call.set_ringback_source")]
    SetRingbackSource {
        target_call_id: Option<String>,
        source_call_id: Option<String>,
    },
    #[serde(rename = "media.play", alias = "MediaPlay")]
    MediaPlay(MediaPlayRequest),
    #[serde(rename = "media.stop")]
    MediaStop { call_id: Option<String> },
    #[serde(rename = "media.stream_start")]
    MediaStreamStart(MediaStreamRequest),
    #[serde(rename = "media.stream_stop")]
    MediaStreamStop { call_id: Option<String> },
    #[serde(rename = "media.inject_start")]
    MediaInjectStart(MediaInjectRequest),
    #[serde(rename = "media.inject_stop")]
    MediaInjectStop { call_id: Option<String> },
    #[serde(rename = "record.start")]
    RecordStart(RecordStartRequest),
    #[serde(rename = "record.pause")]
    RecordPause { call_id: Option<String> },
    #[serde(rename = "record.resume")]
    RecordResume { call_id: Option<String> },
    #[serde(rename = "record.stop")]
    RecordStop { call_id: Option<String> },
    #[serde(rename = "queue.enqueue")]
    QueueEnqueue(QueueEnqueueRequest),
    #[serde(rename = "queue.dequeue")]
    QueueDequeue { call_id: Option<String> },
    #[serde(rename = "queue.hold")]
    QueueHold { call_id: Option<String> },
    #[serde(rename = "queue.unhold")]
    QueueUnhold { call_id: Option<String> },
    #[serde(rename = "queue.set_priority")]
    QueueSetPriority {
        call_id: Option<String>,
        priority: Option<u32>,
    },
    #[serde(rename = "queue.assign_agent")]
    QueueAssignAgent {
        call_id: Option<String>,
        agent_id: Option<String>,
    },
    #[serde(rename = "queue.requeue")]
    QueueRequeue {
        call_id: Option<String>,
        queue_id: Option<String>,
        priority: Option<u32>,
    },
    #[serde(rename = "supervisor.listen")]
    SupervisorListen {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
    },
    #[serde(rename = "supervisor.whisper")]
    SupervisorWhisper {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
        agent_leg: Option<String>,
    },
    #[serde(rename = "supervisor.barge")]
    SupervisorBarge {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
        agent_leg: Option<String>,
    },
    #[serde(rename = "supervisor.takeover")]
    SupervisorTakeover {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
    },
    #[serde(rename = "supervisor.stop")]
    SupervisorStop {
        supervisor_call_id: Option<String>,
        target_call_id: Option<String>,
    },
    #[serde(rename = "sip.message")]
    SipMessage {
        call_id: Option<String>,
        content_type: Option<String>,
        body: Option<String>,
    },
    #[serde(rename = "sip.notify")]
    SipNotify {
        call_id: Option<String>,
        event: Option<String>,
        content_type: Option<String>,
        body: Option<String>,
    },
    #[serde(rename = "sip.options_ping")]
    SipOptionsPing { call_id: Option<String> },
    #[serde(rename = "call.leg_add")]
    LegAdd {
        call_id: Option<String>,
        target: Option<String>,
        leg_id: Option<String>,
    },
    #[serde(rename = "call.leg_remove")]
    LegRemove {
        call_id: Option<String>,
        leg_id: Option<String>,
    },
    #[serde(rename = "call.app_start")]
    AppStart {
        call_id: Option<String>,
        app_name: Option<String>,
        params: Option<serde_json::Value>,
    },
    #[serde(rename = "call.app_stop")]
    AppStop {
        call_id: Option<String>,
        reason: Option<String>,
    },
    #[serde(rename = "conference.create")]
    ConferenceCreate(ConferenceCreateRequest),
    #[serde(rename = "conference.add")]
    ConferenceAdd(ConferenceAddRequest),
    #[serde(rename = "conference.remove")]
    ConferenceRemove(ConferenceRemoveRequest),
    #[serde(rename = "conference.mute")]
    ConferenceMute(ConferenceMuteRequest),
    #[serde(rename = "conference.unmute")]
    ConferenceUnmute(ConferenceUnmuteRequest),
    #[serde(rename = "conference.destroy")]
    ConferenceDestroy(ConferenceDestroyRequest),
    #[serde(rename = "conference.merge")]
    ConferenceMerge(ConferenceMergeRequest),
    #[serde(rename = "conference.seat_replace")]
    ConferenceSeatReplace(ConferenceSeatReplaceRequest),
    #[serde(rename = "session.resume")]
    SessionResume { last_sequence: Option<u64> },
    #[serde(rename = "call.resume")]
    CallResume {
        call_id: Option<String>,
        last_sequence: Option<u64>,
    },
}

impl From<RwiRequest> for RwiCommandPayload {
    fn from(req: RwiRequest) -> Self {
        match req.payload {
            RwiRequestPayload::Subscribe { contexts } => RwiCommandPayload::Subscribe {
                contexts: contexts.unwrap_or_default(),
            },
            RwiRequestPayload::Unsubscribe { contexts } => RwiCommandPayload::Unsubscribe {
                contexts: contexts.unwrap_or_default(),
            },
            RwiRequestPayload::ListCalls => RwiCommandPayload::ListCalls,
            RwiRequestPayload::AttachCall { call_id, mode } => RwiCommandPayload::AttachCall {
                call_id: call_id.unwrap_or_default(),
                mode: match mode.as_deref() {
                    Some("listen") => OwnershipMode::Listen,
                    Some("whisper") => OwnershipMode::Whisper,
                    Some("barge") => OwnershipMode::Barge,
                    _ => OwnershipMode::Control,
                },
            },
            RwiRequestPayload::DetachCall { call_id } => RwiCommandPayload::DetachCall {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::Originate(mut r) => {
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
            RwiRequestPayload::Answer { call_id } => RwiCommandPayload::Answer {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::Reject { call_id, reason } => RwiCommandPayload::Reject {
                call_id: call_id.unwrap_or_default(),
                reason,
            },
            RwiRequestPayload::Ring { call_id } => RwiCommandPayload::Ring {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::Hangup {
                call_id,
                reason,
                code,
            } => RwiCommandPayload::Hangup {
                call_id: call_id.unwrap_or_default(),
                reason,
                code,
            },
            RwiRequestPayload::Bridge { leg_a, leg_b } => RwiCommandPayload::Bridge {
                leg_a: leg_a.unwrap_or_default(),
                leg_b: leg_b.unwrap_or_default(),
            },
            RwiRequestPayload::Unbridge { call_id } => RwiCommandPayload::Unbridge {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::Transfer { call_id, target } => RwiCommandPayload::Transfer {
                call_id: call_id.unwrap_or_default(),
                target: target.unwrap_or_default(),
            },
            RwiRequestPayload::TransferReplace { call_id, target } => {
                RwiCommandPayload::TransferReplace {
                    call_id: call_id.unwrap_or_default(),
                    target: target.unwrap_or_default(),
                }
            }
            RwiRequestPayload::TransferAttended {
                call_id,
                target,
                timeout_secs,
            } => RwiCommandPayload::TransferAttended {
                call_id: call_id.unwrap_or_default(),
                target: target.unwrap_or_default(),
                timeout_secs,
            },
            RwiRequestPayload::TransferComplete {
                call_id,
                consultation_call_id,
            } => RwiCommandPayload::TransferComplete {
                call_id: call_id.unwrap_or_default(),
                consultation_call_id: consultation_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::TransferCancel {
                consultation_call_id,
            } => RwiCommandPayload::TransferCancel {
                consultation_call_id: consultation_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::CallHold { call_id, music } => RwiCommandPayload::CallHold {
                call_id: call_id.unwrap_or_default(),
                music,
            },
            RwiRequestPayload::CallUnhold { call_id } => RwiCommandPayload::CallUnhold {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::SetRingbackSource {
                target_call_id,
                source_call_id,
            } => RwiCommandPayload::SetRingbackSource {
                target_call_id: target_call_id.unwrap_or_default(),
                source_call_id: source_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::MediaPlay(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaPlay(r)
            }
            RwiRequestPayload::MediaStop { call_id } => RwiCommandPayload::MediaStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::MediaStreamStart(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaStreamStart(r)
            }
            RwiRequestPayload::MediaStreamStop { call_id } => RwiCommandPayload::MediaStreamStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::MediaInjectStart(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaInjectStart(r)
            }
            RwiRequestPayload::MediaInjectStop { call_id } => RwiCommandPayload::MediaInjectStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::RecordStart(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::RecordStart(r)
            }
            RwiRequestPayload::RecordPause { call_id } => RwiCommandPayload::RecordPause {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::RecordResume { call_id } => RwiCommandPayload::RecordResume {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::RecordStop { call_id } => RwiCommandPayload::RecordStop {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::QueueEnqueue(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::QueueEnqueue(r)
            }
            RwiRequestPayload::QueueDequeue { call_id } => RwiCommandPayload::QueueDequeue {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::QueueHold { call_id } => RwiCommandPayload::QueueHold {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::QueueUnhold { call_id } => RwiCommandPayload::QueueUnhold {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::QueueSetPriority { call_id, priority } => {
                RwiCommandPayload::QueueSetPriority {
                    call_id: call_id.unwrap_or_default(),
                    priority: priority.unwrap_or(0),
                }
            }
            RwiRequestPayload::QueueAssignAgent { call_id, agent_id } => {
                RwiCommandPayload::QueueAssignAgent {
                    call_id: call_id.unwrap_or_default(),
                    agent_id: agent_id.unwrap_or_default(),
                }
            }
            RwiRequestPayload::QueueRequeue {
                call_id,
                queue_id,
                priority,
            } => RwiCommandPayload::QueueRequeue {
                call_id: call_id.unwrap_or_default(),
                queue_id: queue_id.unwrap_or_default(),
                priority,
            },
            RwiRequestPayload::SupervisorListen {
                supervisor_call_id,
                target_call_id,
            } => RwiCommandPayload::SupervisorListen {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::SupervisorWhisper {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
                agent_leg: agent_leg.unwrap_or_default(),
            },
            RwiRequestPayload::SupervisorBarge {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => RwiCommandPayload::SupervisorBarge {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
                agent_leg: agent_leg.unwrap_or_default(),
            },
            RwiRequestPayload::SupervisorTakeover {
                supervisor_call_id,
                target_call_id,
            } => RwiCommandPayload::SupervisorTakeover {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => RwiCommandPayload::SupervisorStop {
                supervisor_call_id: supervisor_call_id.unwrap_or_default(),
                target_call_id: target_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::SipMessage {
                call_id,
                content_type,
                body,
            } => RwiCommandPayload::SipMessage {
                call_id: call_id.unwrap_or_default(),
                content_type: content_type.unwrap_or_else(|| "text/plain".to_string()),
                body: body.unwrap_or_default(),
            },
            RwiRequestPayload::SipNotify {
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
            RwiRequestPayload::SipOptionsPing { call_id } => RwiCommandPayload::SipOptionsPing {
                call_id: call_id.unwrap_or_default(),
            },
            RwiRequestPayload::LegAdd { call_id, target, leg_id } => {
                RwiCommandPayload::LegAdd {
                    call_id: call_id.unwrap_or_default(),
                    target: target.unwrap_or_default(),
                    leg_id,
                }
            }
            RwiRequestPayload::LegRemove { call_id, leg_id } => RwiCommandPayload::LegRemove {
                call_id: call_id.unwrap_or_default(),
                leg_id: leg_id.unwrap_or_default(),
            },
            RwiRequestPayload::AppStart { call_id, app_name, params } => {
                RwiCommandPayload::AppStart {
                    call_id: call_id.unwrap_or_default(),
                    app_name: app_name.unwrap_or_default(),
                    params,
                }
            }
            RwiRequestPayload::AppStop { call_id, reason } => RwiCommandPayload::AppStop {
                call_id: call_id.unwrap_or_default(),
                reason,
            },
            RwiRequestPayload::ConferenceCreate(mut r) => {
                if r.conf_id.is_empty() {
                    r.conf_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::ConferenceCreate(r)
            }
            RwiRequestPayload::ConferenceAdd(r) => RwiCommandPayload::ConferenceAdd {
                conf_id: r.conf_id.unwrap_or_default(),
                call_id: r.call_id.unwrap_or_default(),
            },
            RwiRequestPayload::ConferenceRemove(r) => RwiCommandPayload::ConferenceRemove {
                conf_id: r.conf_id.unwrap_or_default(),
                call_id: r.call_id.unwrap_or_default(),
            },
            RwiRequestPayload::ConferenceMute(r) => RwiCommandPayload::ConferenceMute {
                conf_id: r.conf_id.unwrap_or_default(),
                call_id: r.call_id.unwrap_or_default(),
            },
            RwiRequestPayload::ConferenceUnmute(r) => RwiCommandPayload::ConferenceUnmute {
                conf_id: r.conf_id.unwrap_or_default(),
                call_id: r.call_id.unwrap_or_default(),
            },
            RwiRequestPayload::ConferenceDestroy(r) => RwiCommandPayload::ConferenceDestroy {
                conf_id: r.conf_id.unwrap_or_default(),
            },
            RwiRequestPayload::ConferenceMerge(r) => RwiCommandPayload::ConferenceMerge {
                conf_id: r.conf_id.unwrap_or_default(),
                call_id: r.call_id.unwrap_or_default(),
                consultation_call_id: r.consultation_call_id.unwrap_or_default(),
            },
            RwiRequestPayload::ConferenceSeatReplace(r) => {
                RwiCommandPayload::ConferenceSeatReplace {
                    conf_id: r.conf_id.unwrap_or_default(),
                    old_call_id: r.old_call_id.unwrap_or_default(),
                    new_call_id: r.new_call_id.unwrap_or_default(),
                }
            }
            RwiRequestPayload::SessionResume { last_sequence } => {
                RwiCommandPayload::SessionResume { last_sequence }
            }
            RwiRequestPayload::CallResume {
                call_id,
                last_sequence,
            } => RwiCommandPayload::CallResume {
                call_id: call_id.unwrap_or_default(),
                last_sequence,
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
}
