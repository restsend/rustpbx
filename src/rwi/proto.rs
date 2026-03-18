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
    },
    MediaStreamStart(MediaStreamParams),
    MediaStreamStop {
        call_id: String,
    },
    MediaInjectStart(MediaInjectParams),
    MediaInjectStop {
        call_id: String,
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
    RecordMaskSegment {
        call_id: String,
        recording_id: String,
        start_secs: u64,
        end_secs: u64,
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
    SupervisorTakeover {
        supervisor_call_id: String,
        target_call_id: String,
        agent_leg: String,
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
pub enum ConferenceBackend {
    Internal,
    External,
}

impl Default for ConferenceBackend {
    fn default() -> Self {
        ConferenceBackend::Internal
    }
}

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
        track_id: String,
    },
    MediaPlayFinished {
        call_id: String,
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
    },
    RecordFailed {
        call_id: String,
        recording_id: String,
        error: String,
    },
    RecordSegmentMasked {
        call_id: String,
        recording_id: String,
        start_secs: u64,
        end_secs: u64,
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
    SupervisorModeStopped {
        supervisor_call_id: String,
        target_call_id: String,
    },
    SupervisorTakeoverCompleted {
        supervisor_call_id: String,
        target_call_id: String,
        previous_agent_call_id: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallIncomingData {
    pub call_id: String,
    pub context: String,
    pub caller: String,
    pub callee: String,
    pub direction: String,
    pub trunk: Option<String>,
    #[serde(default)]
    pub sip_headers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiResponse {
    pub action_id: String,
    pub response: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RwiError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RwiError {
    pub code: String,
    pub message: String,
}

impl RwiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new("invalid_state", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn already_owned() -> Self {
        Self::new("already_owned", "call is owned by another session")
    }

    pub fn forbidden() -> Self {
        Self::new("forbidden", "insufficient scope for this action")
    }

    pub fn rate_limited() -> Self {
        Self::new("rate_limited", "too many requests")
    }
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
    fn test_rwi_error_factory_methods() {
        let err = RwiError::invalid_state("call is not ringing");
        assert_eq!(err.code, "invalid_state");
        assert_eq!(err.message, "call is not ringing");

        let err = RwiError::not_found("call not found");
        assert_eq!(err.code, "not_found");

        let err = RwiError::already_owned();
        assert_eq!(err.code, "already_owned");

        let err = RwiError::forbidden();
        assert_eq!(err.code, "forbidden");

        let err = RwiError::rate_limited();
        assert_eq!(err.code, "rate_limited");
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
    fn test_rwi_response_success() {
        let response = RwiResponse {
            action_id: "test-id".to_string(),
            response: ResponseStatus::Success,
            data: Some(serde_json::json!("call_id")),
            error: None,
        };
        assert!(matches!(response.response, ResponseStatus::Success));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_rwi_response_error() {
        let error = RwiError::not_found("call not found");
        let response = RwiResponse {
            action_id: "test-id".to_string(),
            response: ResponseStatus::Error,
            data: None,
            error: Some(error),
        };
        assert!(matches!(response.response, ResponseStatus::Error));
        assert!(response.error.is_some());
    }

    #[test]
    fn test_call_incoming_data_serialization() {
        let json = r#"{
            "call_id": "c_123",
            "context": "default",
            "caller": "1001",
            "callee": "2000",
            "direction": "inbound"
        }"#;
        let data: CallIncomingData = serde_json::from_str(json).unwrap();
        assert_eq!(data.call_id, "c_123");
        assert_eq!(data.caller, "1001");
        assert_eq!(data.callee, "2000");
        assert_eq!(data.direction, "inbound");
    }

    #[test]
    fn test_response_status_serialization() {
        let json = r#""success""#;
        let status: ResponseStatus = serde_json::from_str(json).unwrap();
        assert!(matches!(status, ResponseStatus::Success));

        let json = r#""error""#;
        let status: ResponseStatus = serde_json::from_str(json).unwrap();
        assert!(matches!(status, ResponseStatus::Error));
    }
}
