use serde::{Deserialize, Serialize};

pub const RWI_VERSION: &str = "1.0";

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
    #[serde(default)]
    pub mcu_uri: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RwiEvent {
    CallIncoming(CallIncomingData),
    CallRinging {
        call_id: String,
    },
    CallEarlyMedia {
        call_id: String,
    },
    CallAnswered {
        call_id: String,
    },
    CallBridged {
        leg_a: String,
        leg_b: String,
    },
    CallUnbridged {
        call_id: String,
    },
    CallTransferred {
        call_id: String,
    },
    CallTransferAccepted {
        call_id: String,
    },
    CallTransferFailed {
        call_id: String,
        sip_status: Option<u16>,
        reason: Option<String>,
    },
    CallHangup {
        call_id: String,
        reason: Option<String>,
        sip_status: Option<u16>,
    },
    CallNoAnswer {
        call_id: String,
    },
    CallBusy {
        call_id: String,
    },
    MediaHoldStarted {
        call_id: String,
    },
    MediaHoldStopped {
        call_id: String,
    },
    MediaRingbackPassthroughStarted {
        source: String,
        target: String,
    },
    MediaRingbackPassthroughStopped {
        source: String,
        target: String,
    },
    MediaPlayStarted {
        call_id: String,
        leg_id: Option<String>,
        track_id: String,
    },
    MediaPlayFinished {
        call_id: String,
        leg_id: Option<String>,
        track_id: String,
        interrupted: bool,
    },
    MediaStreamStarted {
        call_id: String,
    },
    MediaStreamStopped {
        call_id: String,
    },
    RecordStarted {
        call_id: String,
        recording_id: String,
    },
    RecordPaused {
        call_id: String,
        recording_id: String,
    },
    RecordResumed {
        call_id: String,
        recording_id: String,
    },
    RecordStopped {
        call_id: String,
        recording_id: String,
        duration_secs: Option<u64>,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        unique_id: Option<String>,
        #[serde(default)]
        file_size: Option<u64>,
        #[serde(default)]
        download_url: Option<String>,
        #[serde(default)]
        ani: Option<String>,
        #[serde(default)]
        dnis: Option<String>,
        #[serde(default)]
        called_phone: Option<String>,
        #[serde(default)]
        call_type: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        agent_name: Option<String>,
        #[serde(default)]
        call_start_time: Option<String>,
        #[serde(default)]
        call_end_time: Option<String>,
        #[serde(default)]
        upload_time: Option<String>,
        #[serde(default)]
        switch_flag: Option<String>,
        #[serde(default)]
        root_call_id: Option<String>,
    },
    RecordFailed {
        call_id: String,
        recording_id: String,
        error: String,
    },
    RecordingMetadataAvailable {
        call_id: String,
        recording_id: String,
        metadata: RecordingMetadata,
    },
    QueueJoined {
        call_id: String,
        queue_id: String,
    },
    QueuePositionChanged {
        call_id: String,
        queue_id: String,
        position: u32,
    },
    QueueAgentOffered {
        call_id: String,
        queue_id: String,
        agent_id: String,
    },
    QueueAgentConnected {
        call_id: String,
        queue_id: String,
        agent_id: String,
    },
    QueueLeft {
        call_id: String,
        queue_id: String,
        reason: Option<String>,
    },
    QueueWaitTimeout {
        call_id: String,
        queue_id: String,
    },
    QueueOverflowed {
        call_id: String,
        original_queue_id: String,
        overflow_queue_id: String,
        reason: String,
    },
    QueueVoicemailRedirected {
        call_id: String,
        queue_id: String,
        reason: String,
    },
    SupervisorListenStarted {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SupervisorWhisperStarted {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SupervisorBargeStarted {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SupervisorTakeoverStarted {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SupervisorModeStopped {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SipMessageReceived {
        call_id: String,
        content_type: String,
        body: String,
    },
    SipNotifyReceived {
        call_id: String,
        event: String,
        content_type: String,
        body: String,
    },
    Dtmf {
        call_id: String,
        digit: String,
        /// Leg that generated the DTMF
        leg_id: Option<String>,
    },
    /// All requested DTMF digits have been collected (or timeout with ≥ min_digits).
    DtmfCollected {
        call_id: String,
        /// Leg that provided the digits
        leg_id: String,
        /// Collected digit string (terminator excluded)
        digits: String,
    },
    /// DtmfCollect timed out before the minimum number of digits was reached.
    DtmfCollectionTimeout {
        call_id: String,
        leg_id: String,
    },
    ConferenceCreated {
        conf_id: String,
    },
    ConferenceMemberJoined {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberLeft {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberMuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberUnmuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceDestroyed {
        conf_id: String,
    },
    ConferenceError {
        conf_id: String,
        error: String,
    },
    ConferenceConsultDialing {
        call_id: String,
        target: String,
    },
    ConferenceConsultConnected {
        call_id: String,
        target: String,
    },
    ConferenceMergeRequested {
        call_id: String,
        consultation_call_id: String,
    },
    ConferenceMerged {
        conf_id: String,
        call_id: String,
    },
    ConferenceMergeFailed {
        conf_id: String,
        call_id: String,
        reason: String,
    },
    // CC addon events
    AgentStateChanged {
        agent_id: String,
        from_status: String,
        to_status: String,
        call_id: Option<String>,
    },
    QueueCandidatesFound {
        call_id: String,
        queue_id: String,
        candidates: Vec<String>,
        trace_id: String,
    },
    QueueAgentRinging {
        call_id: String,
        queue_id: String,
        agent_id: String,
        trace_id: String,
    },
    QueueAgentNoAnswer {
        call_id: String,
        queue_id: String,
        agent_id: String,
        attempt: u32,
        trace_id: String,
    },
    QueueAgentRejected {
        call_id: String,
        queue_id: String,
        agent_id: String,
        attempt: u32,
        trace_id: String,
    },
    QueueFallbackExecuted {
        call_id: String,
        queue_id: String,
        action: String,
        reason: String,
        trace_id: String,
    },
    QueueAlert {
        queue_id: String,
        alert_type: String,
        message: String,
    },
    ConferenceSeatReplaceStarted {
        conf_id: String,
        old_call_id: String,
        new_call_id: String,
    },
    ConferenceSeatReplaceSucceeded {
        conf_id: String,
        old_call_id: String,
        new_call_id: String,
    },
    ConferenceSeatReplaceFailed {
        conf_id: String,
        old_call_id: String,
        new_call_id: String,
        reason: String,
    },
    ConferenceSeatReplaceRollbackFailed {
        conf_id: String,
        old_call_id: String,
        new_call_id: String,
        reason: String,
    },
    CallOwnershipChanged {
        call_id: String,
        session_id: String,
        mode: String,
    },
    SessionResumed {
        session_id: String,
        last_sequence: u64,
    },
    ParallelOriginateStarted {
        operation_id: String,
        leg_count: u32,
    },
    ParallelOriginateLegRinging {
        operation_id: String,
        call_id: String,
        destination: String,
    },
    ParallelOriginateWinner {
        operation_id: String,
        call_id: String,
        destination: String,
    },
    ParallelOriginateLegCancelled {
        operation_id: String,
        call_id: String,
        reason: String,
    },
    ParallelOriginateCompleted {
        operation_id: String,
        winning_call_id: String,
    },
    ParallelOriginateFailed {
        operation_id: String,
        reason: String,
    },
    // IVR events
    IvrNodeEntered {
        call_id: String,
        node_id: String,
        node_name: String,
        node_type: String,
        app_id: String,
        entry_time: String,
        ani: Option<String>,
        dnis: Option<String>,
        routing_target: Option<String>,
        previous_node_id: Option<String>,
    },
    IvrNodeExited {
        call_id: String,
        node_id: String,
        node_name: String,
        result_value: Option<String>,
        duration_ms: u32,
        exit_time: String,
        next_node_id: Option<String>,
        hangup_reason: Option<String>,
        call_result: Option<String>,
    },
    IvrFlowTransitioned {
        call_id: String,
        from_app_id: String,
        to_app_id: String,
        from_node_id: String,
        to_node_id: String,
        transition_reason: String,
        transition_time: String,
        next_routing_target: Option<String>,
    },
    IvrFlowCompleted {
        call_id: String,
        app_id: String,
        total_nodes_traversed: u32,
        total_duration_ms: u32,
        final_result: String,
        completion_time: String,
        final_routing_target: Option<String>,
    },
    // DN events
    DnStateChanged {
        dn: String,
        event_code: u16,
        event_name: String,
        system_time: String,
        call_id: Option<String>,
        #[serde(default)]
        kz_conn_id: Option<String>,
        agent_id: Option<String>,
        other_dn: Option<String>,
        ani: Option<String>,
        dnis: Option<String>,
        reason_code: Option<String>,
        agent_work_mode: Option<String>,
        releasing_party: Option<String>,
        third_party_dn: Option<String>,
        vq_name: Option<String>,
        routing_target: Option<String>,
        skill_group: Option<String>,
        target_dn: Option<String>,
    },
    DnRegistered {
        dn: String,
        agent_id: Option<String>,
        register_time: String,
    },
    DnUnregistered {
        dn: String,
        agent_id: Option<String>,
        unregister_time: String,
    },
    // Call metadata
    CallMetadataUpdated {
        call_id: String,
        metadata: CallMetadata,
    },
    /// Step-mode IVR debug trace entry
    IvrStepTrace {
        call_id: String,
        session_id: String,
        caller: String,
        callee: String,
        timestamp: String,
        step_index: u32,
        event_type: String,
        action_type: String,
        action_json: Option<String>,
        result_kind: String,
        duration_ms: u64,
        error: Option<String>,
    },
}

impl RwiEvent {
    pub fn call_id(&self) -> Option<&str> {
        match self {
            RwiEvent::CallIncoming(data) => Some(&data.call_id),
            RwiEvent::CallRinging { call_id } => Some(call_id),
            RwiEvent::CallEarlyMedia { call_id } => Some(call_id),
            RwiEvent::CallAnswered { call_id } => Some(call_id),
            RwiEvent::CallUnbridged { call_id } => Some(call_id),
            RwiEvent::CallTransferred { call_id } => Some(call_id),
            RwiEvent::CallTransferAccepted { call_id } => Some(call_id),
            RwiEvent::CallTransferFailed { call_id, .. } => Some(call_id),
            RwiEvent::CallHangup { call_id, .. } => Some(call_id),
            RwiEvent::CallNoAnswer { call_id } => Some(call_id),
            RwiEvent::CallBusy { call_id } => Some(call_id),
            RwiEvent::MediaHoldStarted { call_id } => Some(call_id),
            RwiEvent::MediaHoldStopped { call_id } => Some(call_id),
            RwiEvent::MediaRingbackPassthroughStarted { source, .. } => Some(source),
            RwiEvent::MediaRingbackPassthroughStopped { source, .. } => Some(source),
            RwiEvent::MediaPlayStarted { call_id, .. } => Some(call_id),
            RwiEvent::MediaPlayFinished { call_id, .. } => Some(call_id),
            RwiEvent::MediaStreamStarted { call_id } => Some(call_id),
            RwiEvent::MediaStreamStopped { call_id } => Some(call_id),
            RwiEvent::RecordStarted { call_id, .. } => Some(call_id),
            RwiEvent::RecordPaused { call_id, .. } => Some(call_id),
            RwiEvent::RecordResumed { call_id, .. } => Some(call_id),
            RwiEvent::RecordStopped { call_id, .. } => Some(call_id),
            RwiEvent::RecordFailed { call_id, .. } => Some(call_id),
            RwiEvent::QueueJoined { call_id, .. } => Some(call_id),
            RwiEvent::QueuePositionChanged { call_id, .. } => Some(call_id),
            RwiEvent::QueueAgentOffered { call_id, .. } => Some(call_id),
            RwiEvent::QueueAgentConnected { call_id, .. } => Some(call_id),
            RwiEvent::QueueLeft { call_id, .. } => Some(call_id),
            RwiEvent::QueueWaitTimeout { call_id, .. } => Some(call_id),
            RwiEvent::QueueOverflowed { call_id, .. } => Some(call_id),
            RwiEvent::QueueVoicemailRedirected { call_id, .. } => Some(call_id),
            RwiEvent::SupervisorListenStarted {
                supervisor_call_id, ..
            } => Some(supervisor_call_id),
            RwiEvent::SupervisorWhisperStarted {
                supervisor_call_id, ..
            } => Some(supervisor_call_id),
            RwiEvent::SupervisorBargeStarted {
                supervisor_call_id, ..
            } => Some(supervisor_call_id),
            RwiEvent::SupervisorTakeoverStarted {
                supervisor_call_id, ..
            } => Some(supervisor_call_id),
            RwiEvent::SupervisorModeStopped {
                supervisor_call_id, ..
            } => Some(supervisor_call_id),
            RwiEvent::SipMessageReceived { call_id, .. } => Some(call_id),
            RwiEvent::SipNotifyReceived { call_id, .. } => Some(call_id),
            RwiEvent::Dtmf { call_id, .. } => Some(call_id),
            RwiEvent::DtmfCollected { call_id, .. } => Some(call_id),
            RwiEvent::DtmfCollectionTimeout { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMemberJoined { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMemberLeft { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMemberMuted { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMemberUnmuted { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceConsultDialing { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceConsultConnected { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMergeRequested { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMerged { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceMergeFailed { call_id, .. } => Some(call_id),
            RwiEvent::ConferenceSeatReplaceStarted { old_call_id, .. } => Some(old_call_id),
            RwiEvent::ConferenceSeatReplaceSucceeded { old_call_id, .. } => Some(old_call_id),
            RwiEvent::ConferenceSeatReplaceFailed { old_call_id, .. } => Some(old_call_id),
            RwiEvent::ConferenceSeatReplaceRollbackFailed { old_call_id, .. } => Some(old_call_id),
            RwiEvent::CallOwnershipChanged { call_id, .. } => Some(call_id),

            RwiEvent::ParallelOriginateStarted { .. } => None,
            RwiEvent::ParallelOriginateLegRinging { call_id, .. } => Some(call_id),
            RwiEvent::ParallelOriginateWinner { call_id, .. } => Some(call_id),
            RwiEvent::ParallelOriginateLegCancelled { call_id, .. } => Some(call_id),
            RwiEvent::ParallelOriginateCompleted {
                winning_call_id, ..
            } => Some(winning_call_id),
            RwiEvent::ParallelOriginateFailed { .. } => None,

            RwiEvent::CallBridged { leg_a, .. } => Some(leg_a),
            // CC addon events
            RwiEvent::AgentStateChanged { call_id, .. } => call_id.as_deref(),
            RwiEvent::QueueCandidatesFound { call_id, .. } => Some(call_id),
            RwiEvent::QueueAgentRinging { call_id, .. } => Some(call_id),
            RwiEvent::QueueAgentNoAnswer { call_id, .. } => Some(call_id),
            RwiEvent::QueueAgentRejected { call_id, .. } => Some(call_id),
            RwiEvent::QueueFallbackExecuted { call_id, .. } => Some(call_id),
            RwiEvent::QueueAlert { .. } => None,

            RwiEvent::ConferenceCreated { .. } => None,
            RwiEvent::ConferenceDestroyed { .. } => None,
            RwiEvent::ConferenceError { .. } => None,
            RwiEvent::SessionResumed { .. } => None,

            // IVR events
            RwiEvent::IvrNodeEntered { call_id, .. } => Some(call_id),
            RwiEvent::IvrNodeExited { call_id, .. } => Some(call_id),
            RwiEvent::IvrFlowTransitioned { call_id, .. } => Some(call_id),
            RwiEvent::IvrFlowCompleted { call_id, .. } => Some(call_id),

            // DN events
            RwiEvent::DnStateChanged { call_id, .. } => call_id.as_deref(),
            RwiEvent::DnRegistered { .. } => None,
            RwiEvent::DnUnregistered { .. } => None,

            // Recording metadata
            RwiEvent::RecordingMetadataAvailable { call_id, .. } => Some(call_id),

            // Call metadata
            RwiEvent::CallMetadataUpdated { call_id, .. } => Some(call_id),
            // IVR step trace
            RwiEvent::IvrStepTrace { call_id, .. } => Some(call_id),
        }
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
    pub ani: Option<String>,
    #[serde(default)]
    pub dnis: Option<String>,
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
    pub ani: Option<String>,
    pub dnis: Option<String>,
    pub called_phone: Option<String>,
    pub call_type: String,
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub call_start_time: Option<String>,
    pub call_end_time: Option<String>,
    pub upload_time: Option<String>,
    pub switch_flag: Option<String>,
    pub process_flag: Option<String>,
    pub kz_conn_id: Option<String>,
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
    pub ani: Option<String>,
    pub dnis: Option<String>,
    pub called_phone: Option<String>,
    pub dial_direction: Option<String>,
    pub uuid: Option<String>,
    #[serde(default)]
    pub routing_path: Option<Vec<String>>,
    pub app_id: Option<String>,
    pub routing_target: Option<String>,
    pub switch_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rwi_envelope_new() {
        let envelope = RwiEnvelope::new(RwiCommand::SessionListCalls);
        assert_eq!(envelope.version, RWI_VERSION);
    }

    #[test]
    fn test_attach_mode_serialization() {
        let json = r#""control""#;
        let mode: AttachMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, AttachMode::Control));

        let json = r#""listen""#;
        let mode: AttachMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, AttachMode::Listen));

        let json = r#""whisper""#;
        let mode: AttachMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, AttachMode::Whisper));

        let json = r#""barge""#;
        let mode: AttachMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, AttachMode::Barge));
    }

    #[test]
    fn test_reject_reason_serialization() {
        let json = r#""busy""#;
        let reason: RejectReason = serde_json::from_str(json).unwrap();
        assert!(matches!(reason, RejectReason::Busy));

        let json = r#""forbidden""#;
        let reason: RejectReason = serde_json::from_str(json).unwrap();
        assert!(matches!(reason, RejectReason::Forbidden));

        let json = r#""not_found""#;
        let reason: RejectReason = serde_json::from_str(json).unwrap();
        assert!(matches!(reason, RejectReason::NotFound));
    }

    #[test]
    fn test_ringback_mode_serialization() {
        let json = r#""local""#;
        let mode: RingbackMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, RingbackMode::Local));

        let json = r#""passthrough""#;
        let mode: RingbackMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, RingbackMode::Passthrough));

        let json = r#""none""#;
        let mode: RingbackMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, RingbackMode::None));
    }

    #[test]
    fn test_media_source_serialization() {
        let json = r#"{"type": "file", "uri": "welcome.wav", "looped": true}"#;
        let source: MediaSource = serde_json::from_str(json).unwrap();
        assert!(matches!(source.source_type, MediaSourceType::File));
        assert_eq!(source.uri, Some("welcome.wav".to_string()));
        assert_eq!(source.looped, Some(true));

        let json = r#"{"type": "silence"}"#;
        let source: MediaSource = serde_json::from_str(json).unwrap();
        assert!(matches!(source.source_type, MediaSourceType::Silence));

        let json = r#"{"type": "ringback"}"#;
        let source: MediaSource = serde_json::from_str(json).unwrap();
        assert!(matches!(source.source_type, MediaSourceType::Ringback));
    }

    #[test]
    fn test_media_direction_serialization() {
        let json = r#""send""#;
        let dir: MediaDirection = serde_json::from_str(json).unwrap();
        assert!(matches!(dir, MediaDirection::Send));

        let json = r#""recv""#;
        let dir: MediaDirection = serde_json::from_str(json).unwrap();
        assert!(matches!(dir, MediaDirection::Recv));

        let json = r#""sendrecv""#;
        let dir: MediaDirection = serde_json::from_str(json).unwrap();
        assert!(matches!(dir, MediaDirection::Sendrecv));
    }

    #[test]
    fn test_record_mode_serialization() {
        let json = r#""mixed""#;
        let mode: RecordMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, RecordMode::Mixed));

        let json = r#""separate_legs""#;
        let mode: RecordMode = serde_json::from_str(json).unwrap();
        assert!(matches!(mode, RecordMode::SeparateLegs));
    }

    #[test]
    fn test_call_incoming_data_serialization() {
        let json = r#"{
            "call_id": "c_123",
            "context": "default",
            "caller": "1001",
            "callee": "2000",
            "dial_direction": "inbound",
            "ani": "330909",
            "dnis": "9242000001",
            "called_phone": "018659727661",
            "app_id": "ivr-support-main",
            "routing_target": "queue:support",
            "root_call_id": "call-root-42",
            "uuid": "uuid-abc-123",
            "routing_path": ["menu:root", "queue:level1"]
        }"#;
        let data: CallIncomingData = serde_json::from_str(json).unwrap();
        assert_eq!(data.call_id, "c_123");
        assert_eq!(data.caller, "1001");
        assert_eq!(data.callee, "2000");
        assert_eq!(data.dial_direction, "inbound");
        assert_eq!(data.ani, Some("330909".to_string()));
        assert_eq!(data.dnis, Some("9242000001".to_string()));
        assert_eq!(data.called_phone, Some("018659727661".to_string()));
        assert_eq!(data.app_id, Some("ivr-support-main".to_string()));
        assert_eq!(data.routing_target, Some("queue:support".to_string()));
        assert_eq!(data.root_call_id, Some("call-root-42".to_string()));
        assert_eq!(data.uuid, Some("uuid-abc-123".to_string()));
        assert_eq!(
            data.routing_path,
            Some(vec!["menu:root".into(), "queue:level1".into()])
        );
    }

    #[test]
    fn test_call_incoming_data_defaults() {
        let json = r#"{
            "call_id": "c_456",
            "context": "default",
            "caller": "1002",
            "callee": "2001",
            "dial_direction": "outbound"
        }"#;
        let data: CallIncomingData = serde_json::from_str(json).unwrap();
        assert_eq!(data.call_id, "c_456");
        assert!(data.ani.is_none());
        assert!(data.dnis.is_none());
        assert!(data.called_phone.is_none());
        assert!(data.app_id.is_none());
        assert!(data.routing_target.is_none());
        assert!(data.root_call_id.is_none());
        assert!(data.uuid.is_none());
        assert!(data.routing_path.is_none());
    }

    #[test]
    fn test_seat_replace_events_call_id_mapping() {
        let started = RwiEvent::ConferenceSeatReplaceStarted {
            conf_id: "room-1".to_string(),
            old_call_id: "call-old".to_string(),
            new_call_id: "call-new".to_string(),
        };
        assert_eq!(started.call_id(), Some("call-old"));

        let succeeded = RwiEvent::ConferenceSeatReplaceSucceeded {
            conf_id: "room-1".to_string(),
            old_call_id: "call-old".to_string(),
            new_call_id: "call-new".to_string(),
        };
        assert_eq!(succeeded.call_id(), Some("call-old"));

        let failed = RwiEvent::ConferenceSeatReplaceFailed {
            conf_id: "room-1".to_string(),
            old_call_id: "call-old".to_string(),
            new_call_id: "call-new".to_string(),
            reason: "busy".to_string(),
        };
        assert_eq!(failed.call_id(), Some("call-old"));

        let rollback_failed = RwiEvent::ConferenceSeatReplaceRollbackFailed {
            conf_id: "room-1".to_string(),
            old_call_id: "call-old".to_string(),
            new_call_id: "call-new".to_string(),
            reason: "rollback error".to_string(),
        };
        assert_eq!(rollback_failed.call_id(), Some("call-old"));
    }

    #[test]
    fn test_record_stopped_enhanced_serialization() {
        let json = r#"{
            "record_stopped": {
                "call_id": "call-abc",
                "recording_id": "rec-xyz",
                "duration_secs": 51,
                "filename": "recording_2026-05-14_08-11-49.mp3",
                "unique_id": "0200M6NJ54CGH3AH1K8482LAES4OTFEL",
                "file_size": 149517,
                "download_url": "https://storage.example.com/recording.mp3",
                "ani": "330909",
                "dnis": "9242000001",
                "called_phone": "018659727661",
                "call_type": "outbound",
                "agent_id": "451447",
                "agent_name": "luoxiaofeng90_v",
                "call_start_time": "2026-05-14T08:11:35Z",
                "call_end_time": "2026-05-14T08:12:26Z",
                "upload_time": "2026-05-14T16:14:46Z",
                "switch_flag": "ks",
                "root_call_id": "call-root-42"
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::RecordStopped {
                call_id,
                recording_id,
                duration_secs,
                ref filename,
                ref unique_id,
                file_size,
                ref download_url,
                ref ani,
                ref dnis,
                ref called_phone,
                ref call_type,
                ref agent_id,
                ref agent_name,
                ref call_start_time,
                ref call_end_time,
                ref upload_time,
                ref switch_flag,
                ref root_call_id,
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(recording_id, "rec-xyz");
                assert_eq!(duration_secs, Some(51));
                assert_eq!(filename.as_deref(), Some("recording_2026-05-14_08-11-49.mp3"));
                assert_eq!(unique_id.as_deref(), Some("0200M6NJ54CGH3AH1K8482LAES4OTFEL"));
                assert_eq!(file_size, Some(149517));
                assert_eq!(ani.as_deref(), Some("330909"));
                assert_eq!(dnis.as_deref(), Some("9242000001"));
                assert_eq!(called_phone.as_deref(), Some("018659727661"));
                assert_eq!(call_type.as_deref(), Some("outbound"));
                assert_eq!(agent_id.as_deref(), Some("451447"));
                assert_eq!(root_call_id.as_deref(), Some("call-root-42"));
            }
            _ => panic!("Expected RecordStopped"),
        }
    }

    #[test]
    fn test_record_stopped_legacy_deserialization() {
        let json = r#"{
            "record_stopped": {
                "call_id": "call-abc",
                "recording_id": "rec-xyz",
                "duration_secs": 51
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::RecordStopped {
                call_id,
                recording_id,
                duration_secs,
                ref filename,
                ref unique_id,
                file_size,
                ref download_url,
                ref ani,
                ..
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(recording_id, "rec-xyz");
                assert_eq!(duration_secs, Some(51));
                assert!(filename.is_none());
                assert!(unique_id.is_none());
                assert!(file_size.is_none());
                assert!(download_url.is_none());
                assert!(ani.is_none());
            }
            _ => panic!("Expected RecordStopped"),
        }
    }

    #[test]
    fn test_recording_metadata_available_event() {
        let json = r#"{
            "recording_metadata_available": {
                "call_id": "call-abc",
                "recording_id": "rec-xyz",
                "metadata": {
                    "filename": "rec_20260514.mp3",
                    "unique_id": "uuid-123",
                    "file_size": 149517,
                    "download_url": "https://storage.example.com/rec.mp3",
                    "ani": "330909",
                    "dnis": "9242000001",
                    "called_phone": null,
                    "call_type": "inbound",
                    "agent_id": "451447",
                    "agent_name": "luoxiaofeng90_v",
                    "call_start_time": "2026-05-14T08:11:35Z",
                    "call_end_time": "2026-05-14T08:12:26Z",
                    "upload_time": null,
                    "switch_flag": "ks",
                    "process_flag": "ks_22_normal",
                    "kz_conn_id": null,
                    "root_call_id": null
                }
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::RecordingMetadataAvailable {
                call_id,
                recording_id,
                ref metadata,
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(recording_id, "rec-xyz");
                assert_eq!(metadata.filename, "rec_20260514.mp3");
                assert_eq!(metadata.file_size, 149517);
                assert_eq!(metadata.call_type, "inbound");
            }
            _ => panic!("Expected RecordingMetadataAvailable"),
        }
    }

    #[test]
    fn test_ivr_node_entered_event() {
        let json = r#"{
            "ivr_node_entered": {
                "call_id": "call-abc",
                "node_id": "node-001",
                "node_name": "main_menu.wav",
                "node_type": "menu",
                "app_id": "ivr-support-main",
                "entry_time": "2026-05-14T17:54:45.537Z",
                "ani": "17503062824",
                "dnis": "4000111666",
                "routing_target": "menu:root",
                "previous_node_id": null
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::IvrNodeEntered {
                ref call_id,
                ref node_id,
                ref node_name,
                ref node_type,
                ref app_id,
                ref entry_time,
                ref ani,
                ref dnis,
                ref routing_target,
                ref previous_node_id,
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(node_id, "node-001");
                assert_eq!(node_name, "main_menu.wav");
                assert_eq!(node_type, "menu");
                assert_eq!(app_id, "ivr-support-main");
                assert_eq!(ani.as_deref(), Some("17503062824"));
                assert_eq!(routing_target.as_deref(), Some("menu:root"));
                assert!(previous_node_id.is_none());
            }
            _ => panic!("Expected IvrNodeEntered"),
        }
    }

    #[test]
    fn test_ivr_node_exited_event() {
        let json = r#"{
            "ivr_node_exited": {
                "call_id": "call-abc",
                "node_id": "node-001",
                "node_name": "main_menu.wav",
                "result_value": "1",
                "duration_ms": 4500,
                "exit_time": "2026-05-14T17:54:50.037Z",
                "next_node_id": "node-002",
                "hangup_reason": null,
                "call_result": null
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::IvrNodeExited {
                ref call_id,
                ref node_id,
                ref result_value,
                duration_ms,
                ref next_node_id,
                ..
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(node_id, "node-001");
                assert_eq!(result_value.as_deref(), Some("1"));
                assert_eq!(duration_ms, 4500);
                assert_eq!(next_node_id.as_deref(), Some("node-002"));
            }
            _ => panic!("Expected IvrNodeExited"),
        }
    }

    #[test]
    fn test_ivr_flow_completed_event() {
        let json = r#"{
            "ivr_flow_completed": {
                "call_id": "call-abc",
                "app_id": "ivr-support-main",
                "total_nodes_traversed": 3,
                "total_duration_ms": 15200,
                "final_result": "transferred",
                "completion_time": "2026-05-14T17:55:00.000Z",
                "final_routing_target": "queue:support"
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::IvrFlowCompleted {
                ref call_id,
                ref app_id,
                total_nodes_traversed,
                total_duration_ms,
                ref final_result,
                ..
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(app_id, "ivr-support-main");
                assert_eq!(total_nodes_traversed, 3);
                assert_eq!(total_duration_ms, 15200);
                assert_eq!(final_result, "transferred");
            }
            _ => panic!("Expected IvrFlowCompleted"),
        }
    }

    #[test]
    fn test_dn_state_changed_event() {
        let json = r#"{
            "dn_state_changed": {
                "dn": "80001",
                "event_code": 64,
                "event_name": "ESTABLISHED",
                "system_time": "2026-05-14T17:54:49.003Z",
                "call_id": "call-abc",
                "kz_conn_id": "kc-12345",
                "agent_id": "10001",
                "other_dn": null,
                "ani": "19534519769",
                "dnis": "39989",
                "reason_code": null,
                "agent_work_mode": null,
                "releasing_party": null,
                "third_party_dn": null,
                "vq_name": null,
                "routing_target": null,
                "skill_group": null,
                "target_dn": null
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::DnStateChanged {
                ref dn,
                event_code,
                ref event_name,
                ref call_id,
                ref kz_conn_id,
                ref agent_id,
                ref ani,
                ref dnis,
                ..
            } => {
                assert_eq!(dn, "80001");
                assert_eq!(event_code, 64);
                assert_eq!(event_name, "ESTABLISHED");
                assert_eq!(call_id.as_deref(), Some("call-abc"));
                assert_eq!(kz_conn_id.as_deref(), Some("kc-12345"));
                assert_eq!(agent_id.as_deref(), Some("10001"));
                assert_eq!(ani.as_deref(), Some("19534519769"));
                assert_eq!(dnis.as_deref(), Some("39989"));
            }
            _ => panic!("Expected DnStateChanged"),
        }
    }

    #[test]
    fn test_call_metadata_updated_event() {
        let json = r#"{
            "call_metadata_updated": {
                "call_id": "call-abc",
                "metadata": {
                    "root_call_id": "call-root-42",
                    "ani": "330909",
                    "dnis": "9242000001",
                    "called_phone": "018659727661",
                    "dial_direction": "inbound",
                    "uuid": "uuid-abc-123",
                    "routing_path": ["menu:root", "queue:level1"],
                    "app_id": "ivr-support-main",
                    "routing_target": "queue:support",
                    "switch_name": "SIP_Switch_KS"
                }
            }
        }"#;
        let event: RwiEvent = serde_json::from_str(json).unwrap();
        match event {
            RwiEvent::CallMetadataUpdated {
                ref call_id,
                ref metadata,
            } => {
                assert_eq!(call_id, "call-abc");
                assert_eq!(metadata.ani.as_deref(), Some("330909"));
                assert_eq!(metadata.root_call_id.as_deref(), Some("call-root-42"));
                assert_eq!(metadata.app_id.as_deref(), Some("ivr-support-main"));
                assert_eq!(metadata.switch_name.as_deref(), Some("SIP_Switch_KS"));
                assert_eq!(
                    metadata.routing_path,
                    Some(vec!["menu:root".into(), "queue:level1".into()])
                );
            }
            _ => panic!("Expected CallMetadataUpdated"),
        }
    }

    #[test]
    fn test_all_new_events_call_id_mapping() {
        let ivr_entered = RwiEvent::IvrNodeEntered {
            call_id: "c-1".into(),
            node_id: "n-1".into(),
            node_name: "x".into(),
            node_type: "menu".into(),
            app_id: "a-1".into(),
            entry_time: "t".into(),
            ani: None,
            dnis: None,
            routing_target: None,
            previous_node_id: None,
        };
        assert_eq!(ivr_entered.call_id(), Some("c-1"));

        let ivr_completed = RwiEvent::IvrFlowCompleted {
            call_id: "c-2".into(),
            app_id: "a-1".into(),
            total_nodes_traversed: 3,
            total_duration_ms: 1000,
            final_result: "ok".into(),
            completion_time: "t".into(),
            final_routing_target: None,
        };
        assert_eq!(ivr_completed.call_id(), Some("c-2"));

        let dn_state = RwiEvent::DnStateChanged {
            dn: "8001".into(),
            event_code: 60,
            event_name: "RINGING".into(),
            system_time: "t".into(),
            call_id: Some("c-3".into()),
            kz_conn_id: None,
            agent_id: None,
            other_dn: None,
            ani: None,
            dnis: None,
            reason_code: None,
            agent_work_mode: None,
            releasing_party: None,
            third_party_dn: None,
            vq_name: None,
            routing_target: None,
            skill_group: None,
            target_dn: None,
        };
        assert_eq!(dn_state.call_id(), Some("c-3"));

        let dn_reg = RwiEvent::DnRegistered {
            dn: "8001".into(),
            agent_id: None,
            register_time: "t".into(),
        };
        assert!(dn_reg.call_id().is_none());

        let recording_meta = RwiEvent::RecordingMetadataAvailable {
            call_id: "c-4".into(),
            recording_id: "r-1".into(),
            metadata: RecordingMetadata {
                filename: "f".into(),
                unique_id: "u".into(),
                file_size: 100,
                download_url: None,
                ani: None,
                dnis: None,
                called_phone: None,
                call_type: "inbound".into(),
                agent_id: None,
                agent_name: None,
                call_start_time: None,
                call_end_time: None,
                upload_time: None,
                switch_flag: None,
                process_flag: None,
                kz_conn_id: None,
                root_call_id: None,
            },
        };
        assert_eq!(recording_meta.call_id(), Some("c-4"));

        let call_meta = RwiEvent::CallMetadataUpdated {
            call_id: "c-5".into(),
            metadata: CallMetadata {
                root_call_id: None,
                ani: None,
                dnis: None,
                called_phone: None,
                dial_direction: None,
                uuid: None,
                routing_path: None,
                app_id: None,
                routing_target: None,
                switch_name: None,
            },
        };
        assert_eq!(call_meta.call_id(), Some("c-5"));
    }

    #[test]
    fn test_rwi_event_roundtrip_ivr_node_entered() {
        let original = RwiEvent::IvrNodeEntered {
            call_id: "call-abc".into(),
            node_id: "node-001".into(),
            node_name: "main_menu.wav".into(),
            node_type: "menu".into(),
            app_id: "ivr-support".into(),
            entry_time: "2026-05-14T17:54:45.537Z".into(),
            ani: Some("17503062824".into()),
            dnis: Some("4000111666".into()),
            routing_target: Some("menu:root".into()),
            previous_node_id: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: RwiEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, RwiEvent::IvrNodeEntered { .. }));
        assert_eq!(deserialized.call_id(), original.call_id());
    }

    #[test]
    fn test_rwi_event_roundtrip_dn_state_changed() {
        let original = RwiEvent::DnStateChanged {
            dn: "80001".into(),
            event_code: 64,
            event_name: "ESTABLISHED".into(),
            system_time: "2026-05-14T17:54:49.003Z".into(),
            call_id: Some("call-abc".into()),
            kz_conn_id: Some("kc-12345".into()),
            agent_id: Some("10001".into()),
            other_dn: None,
            ani: Some("19534519769".into()),
            dnis: Some("39989".into()),
            reason_code: None,
            agent_work_mode: None,
            releasing_party: None,
            third_party_dn: None,
            vq_name: None,
            routing_target: None,
            skill_group: None,
            target_dn: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: RwiEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, RwiEvent::DnStateChanged { .. }));
        assert_eq!(deserialized.call_id(), original.call_id());
    }

    #[test]
    fn test_rwi_event_roundtrip_recording_metadata_available() {
        let original = RwiEvent::RecordingMetadataAvailable {
            call_id: "call-abc".into(),
            recording_id: "rec-xyz".into(),
            metadata: RecordingMetadata {
                filename: "recording.mp3".into(),
                unique_id: "uuid-123".into(),
                file_size: 149517,
                download_url: Some("https://storage.example.com/rec.mp3".into()),
                ani: Some("330909".into()),
                dnis: Some("9242000001".into()),
                called_phone: None,
                call_type: "inbound".into(),
                agent_id: Some("451447".into()),
                agent_name: Some("luoxiaofeng90_v".into()),
                call_start_time: Some("2026-05-14T08:11:35Z".into()),
                call_end_time: Some("2026-05-14T08:12:26Z".into()),
                upload_time: Some("2026-05-14T16:14:46Z".into()),
                switch_flag: Some("ks".into()),
                process_flag: Some("ks_22_normal".into()),
                kz_conn_id: None,
                root_call_id: Some("call-root-42".into()),
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: RwiEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, RwiEvent::RecordingMetadataAvailable { .. }),
            "expected RecordingMetadataAvailable, got: {json}"
        );
        assert_eq!(deserialized.call_id(), original.call_id());
    }

    #[test]
    fn test_rwi_event_roundtrip_call_metadata_updated() {
        let original = RwiEvent::CallMetadataUpdated {
            call_id: "call-xyz".into(),
            metadata: CallMetadata {
                root_call_id: Some("call-root-42".into()),
                ani: Some("330909".into()),
                dnis: Some("9242000001".into()),
                called_phone: Some("018659727661".into()),
                dial_direction: Some("inbound".into()),
                uuid: Some("uuid-abc".into()),
                routing_path: Some(vec!["menu:root".into(), "queue:level1".into()]),
                app_id: Some("ivr-support-main".into()),
                routing_target: Some("queue:support".into()),
                switch_name: Some("SIP_Switch_KS".into()),
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: RwiEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, RwiEvent::CallMetadataUpdated { .. }));
        assert_eq!(deserialized.call_id(), Some("call-xyz"));
    }

    #[test]
    fn test_ivr_node_info_serialization() {
        let info = IvrNodeInfo {
            node_id: "n-1".into(),
            node_name: "menu.wav".into(),
            node_type: "menu".into(),
            routing_target: Some("queue:support".into()),
            previous_node_id: Some("n-0".into()),
            next_node_id: Some("n-2".into()),
            duration_ms: Some(5000),
            result_value: Some("1".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: IvrNodeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.node_id, "n-1");
        assert_eq!(deserialized.duration_ms, Some(5000));
    }

    #[test]
    fn test_ivr_flow_context_serialization() {
        let ctx = IvrFlowContext {
            app_id: "ivr-support".into(),
            routing_path: vec!["menu:root".into(), "queue:level1".into()],
            service_type: Some("6".into()),
            customer_type: Some("1".into()),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: IvrFlowContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.app_id, "ivr-support");
        assert_eq!(deserialized.routing_path.len(), 2);
    }

    #[test]
    fn test_ivr_step_trace_roundtrip() {
        let event = RwiEvent::IvrStepTrace {
            call_id: "call_001".into(),
            session_id: "sess_001".into(),
            caller: "1001".into(),
            callee: "2000".into(),
            timestamp: "2026-05-21T16:30:01Z".into(),
            step_index: 1,
            event_type: "provider_response".into(),
            action_type: "Transfer".into(),
            action_json: Some(r#"{"type":"transfer","target":"2001"}"#.into()),
            result_kind: "terminal".into(),
            duration_ms: 42,
            error: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        // With rename_all = "snake_case", variant wraps as "ivr_step_trace"
        let inner = &json["ivr_step_trace"];
        assert_eq!(inner["call_id"], "call_001");
        assert_eq!(inner["step_index"], 1);
        assert_eq!(inner["action_type"], "Transfer");
        assert_eq!(inner["duration_ms"], 42);

        let deserialized: RwiEvent = serde_json::from_value(json).unwrap();
        match &deserialized {
            RwiEvent::IvrStepTrace { call_id, step_index, action_type, .. } => {
                assert_eq!(call_id, "call_001");
                assert_eq!(*step_index, 1);
                assert_eq!(action_type, "Transfer");
            }
            _ => panic!("expected IvrStepTrace"),
        }
    }
}
