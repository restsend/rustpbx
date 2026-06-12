use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const RWI_VERSION: &str = "1.0";

/// Common call context flattened into all call-scoped RWI events.
/// All fields are Option — when None they are omitted from JSON.
/// When enriched, gateway populates from `CallMetaStore`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventCallContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiEnvelope<T> {
    #[serde(rename = "rwi")]
    pub version: String,
    #[serde(flatten)]
    pub payload: T,
}

impl<T> RwiEnvelope<T> {
    pub fn new(payload: T) -> Self {
        Self {
            version: RWI_VERSION.to_string(),
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RwiCommand {
    SessionSubscribe {
        contexts: Vec<String>,
    },
    SessionUnsubscribe {
        contexts: Vec<String>,
    },
    SessionListCalls,
    SessionAttachCall {
        call_id: String,
        mode: AttachMode,
    },
    SessionDetachCall {
        call_id: String,
    },
    CallOriginate(CallOriginateParams),
    CallAnswer {
        call_id: String,
    },
    CallReject {
        call_id: String,
        reason: Option<RejectReason>,
    },
    CallRing {
        call_id: String,
    },
    CallHangup {
        call_id: String,
        reason: Option<String>,
        code: Option<u16>,
    },
    CallBridge {
        leg_a: String,
        leg_b: String,
    },
    CallUnbridge {
        call_id: String,
    },
    CallTransfer {
        call_id: String,
        target: String,
    },
    CallSetRingbackSource {
        target_call_id: String,
        source_call_id: String,
    },
    MediaPlay(MediaPlayParams),
    MediaStop {
        call_id: String,
        /// Target leg (None = all legs)
        leg_id: Option<String>,
    },
    MediaStreamStart(MediaStreamParams),
    MediaStreamStop {
        call_id: String,
    },
    MediaInjectStart(MediaInjectParams),
    MediaInjectStop {
        call_id: String,
    },
    CallSendDtmf {
        call_id: String,
        leg_id: Option<String>,
        digits: String,
    },
    RecordStart(RecordStartParams),
    RecordPause {
        call_id: String,
    },
    RecordResume {
        call_id: String,
    },
    RecordStop {
        call_id: String,
    },
    QueueEnqueue(QueueEnqueueParams),
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
    ConferenceCreate(ConferenceCreateParams),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    Control,
    Listen,
    Whisper,
    Barge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Busy,
    Forbidden,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallOriginateParams {
    pub call_id: String,
    pub destination: String,
    pub caller_id: Option<String>,
    pub timeout_secs: Option<u32>,
    pub hold_music: Option<MediaSource>,
    pub hold_music_target: Option<String>,
    pub ringback: Option<RingbackMode>,
    pub ringback_target: Option<String>,
    #[serde(default)]
    pub extra_headers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingbackMode {
    Local,
    Passthrough,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MediaSource {
    #[serde(rename = "type")]
    pub source_type: MediaSourceType,
    pub uri: Option<String>,
    pub looped: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaSourceType {
    File,
    Silence,
    Ringback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPlayParams {
    pub call_id: String,
    pub source: MediaSource,
    #[serde(default)]
    pub interrupt_on_dtmf: bool,
    /// Target leg (None = all legs)
    #[serde(default)]
    pub leg_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStreamParams {
    pub call_id: String,
    pub direction: MediaDirection,
    pub format: MediaFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaDirection {
    Send,
    Recv,
    Sendrecv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFormat {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub ptime_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInjectParams {
    pub call_id: String,
    pub format: MediaFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordStartParams {
    pub call_id: String,
    pub mode: RecordMode,
    pub beep: Option<bool>,
    pub max_duration_secs: Option<u32>,
    pub storage: RecordStorage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordMode {
    Mixed,
    SeparateLegs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordStorage {
    pub backend: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEnqueueParams {
    pub call_id: String,
    pub queue_id: String,
    pub priority: Option<u32>,
    pub skills: Option<Vec<String>>,
    pub max_wait_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConferenceCreateParams {
    pub conf_id: String,
    #[serde(default)]
    pub backend: ConferenceBackend,
    #[serde(default)]
    pub max_members: Option<u32>,
    #[serde(default)]
    pub record: bool,
    pub mcu_uri: Option<String>,
    #[serde(default)]
    pub host_call_id: Option<String>,
    #[serde(default)]
    pub max_duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ConferenceBackend {
    #[default]
    Internal,
    External,
}

/// Type alias for RWI event sender.
pub type RwiEventTx = tokio::sync::mpsc::UnboundedSender<RwiEvent>;
/// Type alias for RWI event receiver.
pub type RwiEventRx = tokio::sync::mpsc::UnboundedReceiver<RwiEvent>;

#[derive(Debug, Clone, Serialize)]
pub enum RwiEvent {
    Custom(crate::rwi::event::FlatEvent),
}

impl RwiEvent {
    pub fn call_id(&self) -> Option<&str> {
        match self {
            RwiEvent::Custom(flat) => flat.call_id.as_deref(),
        }
    }

    pub fn event_type_name(&self) -> &'static str {
        match self {
            RwiEvent::Custom(flat) => flat.event_type,
        }
    }

    pub fn to_flat_value(&self) -> (Value, &'static str) {
        match self {
            RwiEvent::Custom(flat) => (flat.payload.clone(), flat.event_type),
        }
    }

    pub fn enrich(self, _ctx: EventCallContext) -> Self {
        match self {
            RwiEvent::Custom(flat) => RwiEvent::Custom(flat),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// CallMeta and CallMetaStore
// ═══════════════════════════════════════════════════════════════════════════════

/// Per-call metadata for enriching events at dispatch time.
#[derive(Debug, Clone, Default)]
pub struct CallMeta {
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub direction: Option<String>,
    pub trunk: Option<String>,
    pub app_id: Option<String>,
    pub routing_target: Option<String>,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
}

impl From<CallMeta> for EventCallContext {
    fn from(m: CallMeta) -> Self {
        EventCallContext {
            caller: m.caller,
            callee: m.callee,
            caller_name: m.caller_name,
            callee_name: m.callee_name,
            direction: m.direction,
            trunk: m.trunk,
            app_id: m.app_id,
            routing_target: m.routing_target,
            agent_id: m.agent_id,
            agent_name: m.agent_name,
        }
    }
}

/// Thread-safe, in-memory store mapping call_id → CallMeta.
pub struct CallMetaStore {
    store: RwLock<HashMap<String, CallMeta>>,
}

impl CallMetaStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            store: RwLock::new(HashMap::new()),
        })
    }

    pub async fn insert(&self, call_id: String, meta: CallMeta) {
        self.store.write().await.insert(call_id, meta);
    }

    pub async fn get(&self, call_id: &str) -> Option<CallMeta> {
        self.store.read().await.get(call_id).cloned()
    }

    /// Synchronous non-blocking lookup.
    pub fn get_sync(&self, call_id: &str) -> Option<CallMeta> {
        self.store.try_read().ok()?.get(call_id).cloned()
    }

    pub async fn remove(&self, call_id: &str) {
        self.store.write().await.remove(call_id);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallIncomingData {
    pub call_id: String,
    pub context: String,
    pub caller: String,
    pub callee: String,
    pub dial_direction: String,
    pub trunk: Option<String>,
    #[serde(default)]
    pub sip_headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub root_call_id: Option<String>,
    #[serde(default)]
    pub caller_name: Option<String>,
    #[serde(default)]
    pub callee_name: Option<String>,
    #[serde(default)]
    pub called_phone: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub routing_target: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub routing_path: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecordingMetadata {
    pub filename: String,
    pub unique_id: String,
    pub file_size: u64,
    pub download_url: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub called_phone: Option<String>,
    pub call_type: String,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub call_start_time: Option<String>,
    pub call_end_time: Option<String>,
    pub upload_time: Option<String>,
    pub switch_flag: Option<String>,
    pub process_flag: Option<String>,
    pub root_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IvrNodeInfo {
    pub node_id: String,
    pub node_name: String,
    pub node_type: String,
    pub routing_target: Option<String>,
    pub previous_node_id: Option<String>,
    pub next_node_id: Option<String>,
    pub duration_ms: Option<u32>,
    pub result_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IvrFlowContext {
    pub app_id: String,
    #[serde(default)]
    pub routing_path: Vec<String>,
    pub service_type: Option<String>,
    pub customer_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CallMetadata {
    pub root_call_id: Option<String>,
    pub caller_name: Option<String>,
    pub callee_name: Option<String>,
    pub called_phone: Option<String>,
    pub dial_direction: Option<String>,
    pub uuid: Option<String>,
    #[serde(default)]
    pub routing_path: Option<Vec<String>>,
    pub app_id: Option<String>,
    pub routing_target: Option<String>,
    pub switch_name: Option<String>,
}


