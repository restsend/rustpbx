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

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", content = "params")]
pub enum RwiCommandPayload {
    #[serde(rename = "session.subscribe", alias = "Subscribe")]
    Subscribe {
        #[serde(default)]
        contexts: Vec<String>,
        events: Option<Vec<String>>,
    },
    #[serde(rename = "session.unsubscribe", alias = "Unsubscribe")]
    Unsubscribe {
        #[serde(default)]
        contexts: Vec<String>,
    },
    #[serde(rename = "call.set_var")]
    SetVar {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        key: String,
        #[serde(default)]
        value: String,
    },
    #[serde(rename = "call.get_var")]
    GetVar {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        key: String,
    },
    #[serde(rename = "session.attach_call")]
    AttachCall {
        #[serde(default)]
        call_id: String,
        #[serde(
            default = "default_ownership_mode",
            deserialize_with = "ownership_mode_from_wire"
        )]
        mode: OwnershipMode,
    },
    #[serde(rename = "session.list_calls", alias = "ListCalls")]
    ListCalls,
    #[serde(rename = "session.detach_call")]
    DetachCall {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "call.originate", alias = "Originate")]
    Originate(OriginateRequest),
    #[serde(rename = "call.answer", alias = "Answer")]
    Answer {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "call.reject")]
    Reject {
        #[serde(default)]
        call_id: String,
        reason: Option<String>,
    },
    #[serde(rename = "call.ring")]
    Ring {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "call.hangup")]
    Hangup {
        #[serde(default)]
        call_id: String,
        reason: Option<String>,
        code: Option<u16>,
    },
    #[serde(rename = "call.bridge")]
    Bridge {
        #[serde(default)]
        leg_a: String,
        #[serde(default)]
        leg_b: String,
    },
    #[serde(rename = "call.unbridge")]
    Unbridge {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "call.transfer")]
    Transfer {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        target: String,
    },
    #[serde(rename = "call.transfer.replace")]
    TransferReplace {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        target: String,
    },
    #[serde(rename = "call.transfer.attended")]
    TransferAttended {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        target: String,
        timeout_secs: Option<u32>,
    },
    #[serde(rename = "call.transfer.complete")]
    TransferComplete {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        consultation_call_id: String,
    },
    #[serde(rename = "call.transfer.cancel")]
    TransferCancel {
        #[serde(default)]
        consultation_call_id: String,
    },
    #[serde(rename = "call.hold")]
    CallHold {
        #[serde(default)]
        call_id: String,
        music: Option<String>,
    },
    #[serde(rename = "call.unhold")]
    CallUnhold {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "call.set_ringback_source")]
    SetRingbackSource {
        #[serde(default)]
        target_call_id: String,
        #[serde(default)]
        source_call_id: String,
    },
    #[serde(rename = "media.play", alias = "MediaPlay")]
    MediaPlay(MediaPlayRequest),
    #[serde(rename = "media.stop")]
    MediaStop {
        #[serde(default)]
        call_id: String,
        /// Target leg (None = all legs)
        leg_id: Option<String>,
    },
    #[serde(rename = "call.send_dtmf")]
    CallSendDtmf {
        #[serde(default)]
        call_id: String,
        leg_id: Option<String>,
        #[serde(default)]
        digits: String,
    },
    #[serde(rename = "dtmf.collect")]
    DtmfCollect(DtmfCollectRequest),
    #[serde(rename = "record.start")]
    RecordStart(RecordStartRequest),
    #[serde(rename = "record.pause")]
    RecordPause {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "record.resume")]
    RecordResume {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "record.stop")]
    RecordStop {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "queue.enqueue")]
    QueueEnqueue(QueueEnqueueRequest),
    #[serde(rename = "queue.dequeue")]
    QueueDequeue {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "queue.hold")]
    QueueHold {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "queue.unhold")]
    QueueUnhold {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "queue.set_priority")]
    QueueSetPriority {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        priority: u32,
    },
    #[serde(rename = "queue.assign_agent")]
    QueueAssignAgent {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        agent_id: String,
    },
    #[serde(rename = "queue.requeue")]
    QueueRequeue {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        queue_id: String,
        priority: Option<u32>,
    },
    #[serde(rename = "supervisor.listen")]
    SupervisorListen {
        #[serde(default)]
        supervisor_call_id: String,
        #[serde(default)]
        target_call_id: String,
    },
    #[serde(rename = "supervisor.whisper")]
    SupervisorWhisper {
        #[serde(default)]
        supervisor_call_id: String,
        #[serde(default)]
        target_call_id: String,
        #[serde(default)]
        agent_leg: String,
    },
    #[serde(rename = "supervisor.barge")]
    SupervisorBarge {
        #[serde(default)]
        supervisor_call_id: String,
        #[serde(default)]
        target_call_id: String,
        #[serde(default)]
        agent_leg: String,
    },
    #[serde(rename = "supervisor.takeover")]
    SupervisorTakeover {
        #[serde(default)]
        supervisor_call_id: String,
        #[serde(default)]
        target_call_id: String,
    },
    #[serde(rename = "supervisor.stop")]
    SupervisorStop {
        #[serde(default)]
        supervisor_call_id: String,
        #[serde(default)]
        target_call_id: String,
    },
    #[serde(rename = "sip.message")]
    SipMessage {
        #[serde(default)]
        call_id: String,
        #[serde(default = "default_text_plain")]
        content_type: String,
        #[serde(default)]
        body: String,
    },
    #[serde(rename = "sip.notify")]
    SipNotify {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        event: String,
        #[serde(default = "default_application_json")]
        content_type: String,
        #[serde(default)]
        body: String,
    },
    #[serde(rename = "sip.options_ping")]
    SipOptionsPing {
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "call.leg_add")]
    LegAdd {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        target: String,
        leg_id: Option<String>,
    },
    #[serde(rename = "call.leg_remove")]
    LegRemove {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        leg_id: String,
    },
    #[serde(rename = "call.app_start")]
    AppStart {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        app_name: String,
        params: Option<serde_json::Value>,
    },
    #[serde(rename = "call.app_stop")]
    AppStop {
        #[serde(default)]
        call_id: String,
        reason: Option<String>,
    },
    #[serde(rename = "app.chain", alias = "AppChain")]
    AppChain {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        app_name: String,
        params: Option<serde_json::Value>,
    },
    #[serde(rename = "conference.create")]
    ConferenceCreate(ConferenceCreateRequest),
    #[serde(rename = "conference.add")]
    ConferenceAdd {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "conference.remove")]
    ConferenceRemove {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "conference.mute")]
    ConferenceMute {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "conference.unmute")]
    ConferenceUnmute {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        call_id: String,
    },
    #[serde(rename = "conference.destroy")]
    ConferenceDestroy {
        #[serde(default)]
        conf_id: String,
    },
    #[serde(rename = "conference.end")]
    ConferenceEnd {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        host_call_id: String,
    },
    #[serde(rename = "conference.merge")]
    ConferenceMerge {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        consultation_call_id: String,
    },
    #[serde(rename = "conference.seat_replace")]
    ConferenceSeatReplace {
        #[serde(default)]
        conf_id: String,
        #[serde(default)]
        old_call_id: String,
        #[serde(default)]
        new_call_id: String,
    },
    #[serde(rename = "session.resume")]
    SessionResume { last_sequence: Option<u64> },
    #[serde(rename = "call.resume")]
    CallResume {
        #[serde(default)]
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
            | RwiCommandPayload::SetRingbackSource {
                target_call_id: call_id,
                ..
            }
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
            | RwiCommandPayload::SupervisorListen {
                target_call_id: call_id,
                ..
            }
            | RwiCommandPayload::SupervisorWhisper {
                target_call_id: call_id,
                ..
            }
            | RwiCommandPayload::SupervisorBarge {
                target_call_id: call_id,
                ..
            }
            | RwiCommandPayload::SupervisorTakeover {
                target_call_id: call_id,
                ..
            }
            | RwiCommandPayload::SupervisorStop {
                target_call_id: call_id,
                ..
            }
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
            RwiCommandPayload::SetVar { call_id, .. }
            | RwiCommandPayload::GetVar { call_id, .. } => Some(call_id.as_str()),
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
    /// Recording control: when present, recording starts automatically once
    /// the originated call is answered (same parameters as `record.start`;
    /// `call_id` inside is ignored). `storage.path` may be empty to use the
    /// default recorder file location (`[recording].path/<call_id>.wav`).
    #[serde(default)]
    pub record: Option<RecordStartRequest>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

// ── serde helpers for the wire format ─────────────────────────────────────

fn default_text_plain() -> String {
    "text/plain".to_string()
}

fn default_application_json() -> String {
    "application/json".to_string()
}

fn default_ownership_mode() -> OwnershipMode {
    OwnershipMode::Control
}

/// Wire mode strings are lowercase ("listen"/"whisper"/"barge"); anything
/// else (including "control" and unknown values) maps to Control.
fn ownership_mode_from_wire<'de, D>(deserializer: D) -> Result<OwnershipMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    Ok(match s.as_deref() {
        Some("listen") => OwnershipMode::Listen,
        Some("whisper") => OwnershipMode::Whisper,
        Some("barge") => OwnershipMode::Barge,
        _ => OwnershipMode::Control,
    })
}

impl RwiCommandPayload {
    /// Post-deserialization fixups that used to live in the old wire→internal
    /// conversion: generate call/conference ids when the client omitted them.
    pub fn normalize(&mut self) {
        match self {
            Self::Originate(r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
                // Historical quirk preserved for wire compatibility: the
                // WS layer has always dropped extra_headers from originate
                // requests (the SIP-side headers come from the dialplan).
                r.extra_headers = HashMap::new();
            }
            Self::MediaPlay(r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
            }
            Self::RecordStart(r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
            }
            Self::QueueEnqueue(r) => {
                if r.call_id.is_empty() {
                    r.call_id = Uuid::new_v4().to_string();
                }
            }
            Self::ConferenceCreate(r) => {
                if r.conf_id.is_empty() {
                    r.conf_id = Uuid::new_v4().to_string();
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RwiRequest {
    pub action_id: String,
    #[serde(flatten)]
    pub payload: RwiCommandPayload,
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

        assert!(session.owns_call("call-001"));
        assert!(session.owns_call("call-002"));
        assert!(!session.owns_call("call-003"));
    }
}
