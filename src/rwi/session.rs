use crate::rwi::auth::RwiIdentity;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RwiSession {
    pub id: String,
    pub identity: RwiIdentity,
    pub subscribed_contexts: HashSet<String>,
    pub owned_calls: HashMap<String, CallOwnership>,
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

#[derive(Debug, Clone)]
pub enum RwiCommandPayload {
    Subscribe {
        contexts: Vec<String>,
        events: Option<Vec<String>>,
    },
    Unsubscribe {
        contexts: Vec<String>,
    },
    SetVar {
        call_id: String,
        key: String,
        value: String,
    },
    GetVar {
        call_id: String,
        key: String,
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
        /// Target leg (None = all legs)
        leg_id: Option<String>,
    },
    CallSendDtmf {
        call_id: String,
        leg_id: Option<String>,
        digits: String,
    },
    DtmfCollect(DtmfCollectRequest),
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
    AppChain {
        call_id: String,
        app_name: String,
        params: Option<serde_json::Value>,
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
    ConferenceEnd {
        conf_id: String,
        host_call_id: String,
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
    SessionResume {
        last_sequence: Option<u64>,
    },
    CallResume {
        call_id: String,
        last_sequence: Option<u64>,
    },
}

impl RwiCommandPayload {
    /// The call/session id this command targets, used to route event delivery
    /// and the unified-dispatch path. Conference commands return `None`: they
    /// are handled at the processor level (ConferenceManager) and never flow
    /// through the session command dispatch.
    pub fn dispatch_call_id(&self) -> Option<&str> {
        match self {
            RwiCommandPayload::Answer { call_id }
            | RwiCommandPayload::Hangup { call_id, .. }
            | RwiCommandPayload::Reject { call_id, .. }
            | RwiCommandPayload::Ring { call_id }
            | RwiCommandPayload::CallHold { call_id, .. }
            | RwiCommandPayload::CallUnhold { call_id }
            | RwiCommandPayload::Unbridge { call_id }
            | RwiCommandPayload::Transfer { call_id, .. }
            | RwiCommandPayload::TransferReplace { call_id, .. }
            | RwiCommandPayload::TransferAttended { call_id, .. }
            | RwiCommandPayload::TransferComplete { call_id, .. }
            | RwiCommandPayload::SetRingbackSource { target_call_id: call_id, .. }
            | RwiCommandPayload::MediaStop { call_id, .. }
            | RwiCommandPayload::AttachCall { call_id, .. }
            | RwiCommandPayload::DetachCall { call_id }
            | RwiCommandPayload::RecordPause { call_id }
            | RwiCommandPayload::RecordResume { call_id }
            | RwiCommandPayload::RecordStop { call_id }
            | RwiCommandPayload::QueueDequeue { call_id }
            | RwiCommandPayload::QueueHold { call_id }
            | RwiCommandPayload::QueueUnhold { call_id }
            | RwiCommandPayload::QueueSetPriority { call_id, .. }
            | RwiCommandPayload::QueueAssignAgent { call_id, .. }
            | RwiCommandPayload::QueueRequeue { call_id, .. }
            | RwiCommandPayload::SupervisorListen { target_call_id: call_id, .. }
            | RwiCommandPayload::SupervisorWhisper { target_call_id: call_id, .. }
            | RwiCommandPayload::SupervisorBarge { target_call_id: call_id, .. }
            | RwiCommandPayload::SupervisorTakeover { target_call_id: call_id, .. }
            | RwiCommandPayload::SupervisorStop { target_call_id: call_id, .. }
            | RwiCommandPayload::SipMessage { call_id, .. }
            | RwiCommandPayload::SipNotify { call_id, .. }
            | RwiCommandPayload::SipOptionsPing { call_id }
            | RwiCommandPayload::LegAdd { call_id, .. }
            | RwiCommandPayload::LegRemove { call_id, .. }
            | RwiCommandPayload::CallResume { call_id, .. } => Some(call_id.as_str()),
            RwiCommandPayload::Bridge { leg_a, .. } => Some(leg_a.as_str()),
            RwiCommandPayload::TransferCancel {
                consultation_call_id,
            } => Some(consultation_call_id.as_str()),
            RwiCommandPayload::MediaPlay(req) => Some(req.call_id.as_str()),
            RwiCommandPayload::Originate(req) => Some(req.call_id.as_str()),
            RwiCommandPayload::RecordStart(req) => Some(req.call_id.as_str()),
            RwiCommandPayload::QueueEnqueue(req) => Some(req.call_id.as_str()),
            RwiCommandPayload::DtmfCollect(req) => Some(req.call_id.as_str()),
            RwiCommandPayload::SetVar { call_id, .. } | RwiCommandPayload::GetVar { call_id, .. } => {
                Some(call_id.as_str())
            }
            RwiCommandPayload::CallSendDtmf { call_id, .. }
            | RwiCommandPayload::AppStart { call_id, .. }
            | RwiCommandPayload::AppStop { call_id, .. }
            | RwiCommandPayload::AppChain { call_id, .. } => Some(call_id.as_str()),
            RwiCommandPayload::ConferenceCreate(_)
            | RwiCommandPayload::ConferenceDestroy { .. }
            | RwiCommandPayload::ConferenceEnd { .. }
            | RwiCommandPayload::ConferenceSeatReplace { .. }
            | RwiCommandPayload::ConferenceAdd { .. }
            | RwiCommandPayload::ConferenceRemove { .. }
            | RwiCommandPayload::ConferenceMute { .. }
            | RwiCommandPayload::ConferenceUnmute { .. }
            | RwiCommandPayload::ConferenceMerge { .. }
            | RwiCommandPayload::Subscribe { .. }
            | RwiCommandPayload::Unsubscribe { .. }
            | RwiCommandPayload::ListCalls
            | RwiCommandPayload::SessionResume { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OriginateRequest {
    #[serde(default)]
    pub call_id: String,
    #[serde(default)]
    pub destination: String,
    pub caller_id: Option<String>,
    pub timeout_secs: Option<u32>,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    /// Optional explicit carrier-trunk override. When set, the originate is routed
    /// out the named trunk directly: the trunk's next-hop destination, transport,
    /// digest credential, host rewrite, and P-Asserted-Identity header are stamped
    /// onto the outbound INVITE (so it goes straight to the carrier). When absent,
    /// no route table is consulted and the legacy direct-to-callee behavior is
    /// preserved (a registered/reachable SIP URI). The API caller selects the trunk
    /// by name; the named trunk's config is applied here. See originate_call /
    /// apply_explicit_originate_trunk for the security/admission caveats.
    #[serde(default)]
    pub trunk: Option<String>,
    /// Per-request override for routing the originate through the route table
    /// (match/rewrite/trunk selection). Takes precedence over the session-level
    /// dialplan flag and the global `ProxyConfig.route_originated_calls` default.
    /// An explicit `trunk` wins over this field. `None` falls back to the global
    /// default.
    #[serde(default)]
    pub route_originated_calls: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MediaSource {
    #[serde(default, alias = "type")]
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
    /// Target leg (None = all legs)
    #[serde(default)]
    pub leg_id: Option<String>,
    #[serde(default, alias = "loop")]
    pub loop_playback: bool,
}

/// Request to collect DTMF digits from a call leg.
#[derive(Debug, Clone, Deserialize)]
pub struct DtmfCollectRequest {
    #[serde(default)]
    pub call_id: String,
    /// Target leg (None = caller)
    #[serde(default)]
    pub leg_id: Option<String>,
    /// Minimum digits before a timeout counts as a successful collection
    #[serde(default = "default_min_digits")]
    pub min_digits: u32,
    /// Maximum digits to collect before stopping
    #[serde(default = "default_max_digits")]
    pub max_digits: u32,
    /// Inter-digit / overall timeout in milliseconds
    #[serde(default = "default_dtmf_timeout_ms")]
    pub timeout_ms: u64,
    /// Optional terminator digit (not included in result)
    pub terminator: Option<char>,
}

fn default_min_digits() -> u32 {
    1
}
fn default_max_digits() -> u32 {
    16
}
fn default_dtmf_timeout_ms() -> u64 {
    10_000
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
    pub path: String,
}

impl Default for RecordStorage {
    fn default() -> Self {
        Self {
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
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConferenceCreateRequest {
    #[serde(default, alias = "conference_id")]
    pub conf_id: String,
    pub max_members: Option<u32>,
    pub host_call_id: Option<String>,
    pub max_duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConferenceMemberRequest {
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
    Subscribe {
        contexts: Option<Vec<String>>,
        #[serde(default)]
        events: Option<Vec<String>>,
    },
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
    #[serde(rename = "call.set_var")]
    SetVar {
        call_id: Option<String>,
        key: Option<String>,
        value: Option<String>,
    },
    #[serde(rename = "call.get_var")]
    GetVar {
        call_id: Option<String>,
        key: Option<String>,
    },
    #[serde(rename = "media.play", alias = "MediaPlay")]
    MediaPlay(MediaPlayRequest),
    #[serde(rename = "media.stop")]
    MediaStop {
        call_id: Option<String>,
        leg_id: Option<String>,
    },
    #[serde(rename = "call.send_dtmf")]
    CallSendDtmf {
        call_id: Option<String>,
        leg_id: Option<String>,
        digits: Option<String>,
    },
    #[serde(rename = "dtmf.collect")]
    DtmfCollect(DtmfCollectRequest),
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
    #[serde(rename = "app.chain", alias = "AppChain")]
    AppChain {
        call_id: Option<String>,
        app_name: Option<String>,
        params: Option<serde_json::Value>,
    },
    #[serde(rename = "conference.create")]
    ConferenceCreate(ConferenceCreateRequest),
    #[serde(rename = "conference.add")]
    ConferenceAdd(ConferenceMemberRequest),
    #[serde(rename = "conference.remove")]
    ConferenceRemove(ConferenceMemberRequest),
    #[serde(rename = "conference.mute")]
    ConferenceMute(ConferenceMemberRequest),
    #[serde(rename = "conference.unmute")]
    ConferenceUnmute(ConferenceMemberRequest),
    #[serde(rename = "conference.destroy")]
    ConferenceDestroy(ConferenceDestroyRequest),
    #[serde(rename = "conference.end")]
    ConferenceEnd {
        #[serde(alias = "conference_id")]
        conf_id: Option<String>,
        host_call_id: Option<String>,
    },
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
            RwiRequestPayload::Subscribe { contexts, events } => RwiCommandPayload::Subscribe {
                contexts: contexts.unwrap_or_default(),
                events,
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
            RwiRequestPayload::SetVar {
                call_id,
                key,
                value,
            } => RwiCommandPayload::SetVar {
                call_id: call_id.unwrap_or_default(),
                key: key.unwrap_or_default(),
                value: value.unwrap_or_default(),
            },
            RwiRequestPayload::GetVar { call_id, key } => RwiCommandPayload::GetVar {
                call_id: call_id.unwrap_or_default(),
                key: key.unwrap_or_default(),
            },
            RwiRequestPayload::MediaPlay(mut r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                RwiCommandPayload::MediaPlay(r)
            }
            RwiRequestPayload::MediaStop { call_id, leg_id } => RwiCommandPayload::MediaStop {
                call_id: call_id.unwrap_or_default(),
                leg_id,
            },
            RwiRequestPayload::CallSendDtmf {
                call_id,
                leg_id,
                digits,
            } => RwiCommandPayload::CallSendDtmf {
                call_id: call_id.unwrap_or_default(),
                leg_id,
                digits: digits.unwrap_or_default(),
            },
            RwiRequestPayload::DtmfCollect(r) => RwiCommandPayload::DtmfCollect(r),
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
            RwiRequestPayload::LegAdd {
                call_id,
                target,
                leg_id,
            } => RwiCommandPayload::LegAdd {
                call_id: call_id.unwrap_or_default(),
                target: target.unwrap_or_default(),
                leg_id,
            },
            RwiRequestPayload::LegRemove { call_id, leg_id } => RwiCommandPayload::LegRemove {
                call_id: call_id.unwrap_or_default(),
                leg_id: leg_id.unwrap_or_default(),
            },
            RwiRequestPayload::AppStart {
                call_id,
                app_name,
                params,
            } => RwiCommandPayload::AppStart {
                call_id: call_id.unwrap_or_default(),
                app_name: app_name.unwrap_or_default(),
                params,
            },
            RwiRequestPayload::AppStop { call_id, reason } => RwiCommandPayload::AppStop {
                call_id: call_id.unwrap_or_default(),
                reason,
            },
            RwiRequestPayload::AppChain {
                call_id,
                app_name,
                params,
            } => RwiCommandPayload::AppChain {
                call_id: call_id.unwrap_or_default(),
                app_name: app_name.unwrap_or_default(),
                params,
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
            RwiRequestPayload::ConferenceEnd {
                conf_id,
                host_call_id,
            } => RwiCommandPayload::ConferenceEnd {
                conf_id: conf_id.unwrap_or_default(),
                host_call_id: host_call_id.unwrap_or_default(),
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
    pub fn new(identity: RwiIdentity) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            identity,
            subscribed_contexts: HashSet::new(),
            owned_calls: HashMap::new(),
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

    pub fn list_owned_calls(&self) -> Vec<String> {
        self.owned_calls.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rwi::auth::RwiIdentity;

    fn create_test_identity() -> RwiIdentity {
        RwiIdentity {
            token: "test-token".to_string(),
            scopes: vec!["call.control".to_string()],
        }
    }

    fn create_test_session() -> RwiSession {
        let identity = create_test_identity();
        RwiSession::new(identity)
    }

    #[test]
    fn test_session_creation() {
        let identity = create_test_identity();
        let session = RwiSession::new(identity.clone());

        assert!(!session.id.is_empty());
        assert_eq!(session.identity.token, "test-token");
        assert!(session.subscribed_contexts.is_empty());
        assert!(session.owned_calls.is_empty());
    }

    #[test]
    fn test_subscribe() {
        let mut session = create_test_session();
        session.subscribe(vec!["context1".to_string(), "context2".to_string()]);

        assert!(session.subscribed_contexts.contains("context1"));
        assert!(session.subscribed_contexts.contains("context2"));
        assert_eq!(session.subscribed_contexts.len(), 2);
    }

    #[test]
    fn test_unsubscribe() {
        let mut session = create_test_session();
        session.subscribe(vec!["context1".to_string(), "context2".to_string()]);
        session.unsubscribe(&["context1".to_string()]);

        assert!(!session.subscribed_contexts.contains("context1"));
        assert!(session.subscribed_contexts.contains("context2"));
    }

    #[test]
    fn test_claim_call() {
        let mut session = create_test_session();

        let result = session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(result);
        assert!(session.owns_call("call-001"));

        let result = session.claim_call("call-001".to_string(), OwnershipMode::Control);
        assert!(!result);
    }

    #[test]
    fn test_release_call() {
        let mut session = create_test_session();

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
        let mut session = create_test_session();

        session.claim_call("call-001".to_string(), OwnershipMode::Control);
        session.claim_call("call-002".to_string(), OwnershipMode::Listen);

        let calls = session.list_owned_calls();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&"call-001".to_string()));
        assert!(calls.contains(&"call-002".to_string()));
    }

}
