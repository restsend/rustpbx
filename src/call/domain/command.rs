//! Unified CallCommand - the single command type for session control
//!
//! This enum represents all possible commands that can be sent to a session.
//! It serves as the unified interface between:
//! - RWI (Realtime WebSocket Interface)
//! - Console/HTTP API
//! - Internal event handling
//!
//! ## Design Notes
//!
//! 1. Commands are protocol-agnostic - adapters translate from external protocols
//! 2. Each command has explicit leg targeting via `LegId`
//! 3. Media commands include capability-aware options

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::{HangupCommand, LegId, MediaSource, RingbackPolicy};

/// Type alias for CallCommand sender.
pub type CallCommandTx = mpsc::UnboundedSender<CallCommand>;
/// Type alias for CallCommand receiver.
pub type CallCommandRx = mpsc::UnboundedReceiver<CallCommand>;

/// Unified command for session control
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallCommand {
    // ============================================================================
    // Basic Call Control
    // ============================================================================
    /// Answer an incoming call leg
    Answer {
        /// The leg to answer
        leg_id: LegId,
    },

    /// Reject an incoming call leg
    Reject {
        /// The leg to reject
        leg_id: LegId,
        /// Optional rejection reason
        reason: Option<String>,
    },

    /// Start ringing indication (send 180 Ringing)
    Ring {
        /// The leg to ring
        leg_id: LegId,
        /// Ringback policy (how to handle ringback tone)
        ringback: Option<RingbackPolicy>,
    },

    /// Hangup the session or a specific leg
    Hangup(HangupCommand),

    /// Bridge two legs together
    Bridge {
        /// First leg (A-leg)
        leg_a: LegId,
        /// Second leg (B-leg)
        leg_b: LegId,
        /// Bridge mode
        mode: P2PMode,
    },

    /// Remove a leg from its bridge
    Unbridge {
        /// The leg to unbridge
        leg_id: LegId,
    },

    /// Bridge two legs from different sessions (cross-session P2P).
    /// Used when downgrading from a conference to P2P after transfer completion.
    BridgeCrossSession {
        /// First session ID
        session_a: String,
        /// First leg ID within session_a
        leg_a: LegId,
        /// Second session ID
        session_b: String,
        /// Second leg ID within session_b
        leg_b: LegId,
    },

    /// Transfer a leg to a target (blind transfer)
    Transfer {
        /// The leg to transfer
        leg_id: LegId,
        /// Transfer target (SIP URI or endpoint)
        target: String,
        /// Whether this is an attended transfer
        attended: bool,
    },

    /// Complete an attended transfer
    TransferComplete {
        /// The consultation leg
        consult_leg: LegId,
    },

    /// Cancel an attended transfer
    TransferCancel {
        /// The consultation leg to hangup
        consult_leg: LegId,
    },

    /// Complete a cross-session attended transfer by migrating a leg into a conference.
    /// This is used in the BC -> ABC conference flow where leg_c from session2
    /// needs to be migrated into a conference that also includes legs from session1.
    TransferCompleteCrossSession {
        /// The session ID containing the leg to migrate
        from_session: String,
        /// The leg ID within from_session to migrate
        leg_id: LegId,
        /// The target conference ID to migrate the leg into
        into_conference: String,
    },

    /// Place a leg on hold
    Hold {
        /// The leg to hold
        leg_id: LegId,
        /// Optional music source to play while on hold
        music: Option<MediaSource>,
    },

    /// Release a leg from hold
    Unhold {
        /// The leg to unhold
        leg_id: LegId,
    },

    /// Play audio to a leg or all legs
    Play {
        /// Target leg (None = all legs)
        leg_id: Option<LegId>,
        /// Audio source
        source: MediaSource,
        /// Playback options
        options: Option<PlayOptions>,
    },

    /// Stop audio playback
    StopPlayback {
        /// Target leg (None = all legs)
        leg_id: Option<LegId>,
    },

    /// Send DTMF digits
    SendDtmf {
        /// Target leg
        leg_id: LegId,
        /// DTMF digits to send
        digits: String,
    },

    /// Start recording
    StartRecording {
        /// Recording configuration
        config: RecordConfig,
    },

    /// Pause recording
    PauseRecording,

    /// Resume recording
    ResumeRecording,

    /// Stop recording
    StopRecording,

    /// Supervisor listen mode (monitoring only)
    SupervisorListen {
        /// Supervisor's leg (or supervisor session ID for cross-session monitoring)
        supervisor_leg: LegId,
        /// Target leg to monitor
        target_leg: LegId,
        /// Optional supervisor session ID when monitoring from a different session
        supervisor_session_id: Option<String>,
    },

    /// Supervisor whisper mode (can talk to agent only)
    SupervisorWhisper {
        /// Supervisor's leg (or supervisor session ID for cross-session monitoring)
        supervisor_leg: LegId,
        /// Target leg (agent)
        target_leg: LegId,
        /// Optional supervisor session ID when monitoring from a different session
        supervisor_session_id: Option<String>,
    },

    /// Supervisor barge mode (join conversation)
    SupervisorBarge {
        /// Supervisor's leg (or supervisor session ID for cross-session monitoring)
        supervisor_leg: LegId,
        /// Target leg (agent)
        target_leg: LegId,
        /// Optional supervisor session ID when monitoring from a different session
        supervisor_session_id: Option<String>,
    },

    /// Supervisor takeover mode (replace agent)
    SupervisorTakeover {
        /// Supervisor's leg (or supervisor session ID for cross-session monitoring)
        supervisor_leg: LegId,
        /// Target leg (agent to be replaced)
        target_leg: LegId,
        /// Optional supervisor session ID when monitoring from a different session
        supervisor_session_id: Option<String>,
    },

    /// Stop supervisor mode
    SupervisorStop {
        /// Supervisor's leg
        supervisor_leg: LegId,
    },

    /// Create a conference
    ConferenceCreate {
        /// Conference ID
        conf_id: String,
        /// Conference options
        options: ConferenceOptions,
    },

    /// Add a leg to a conference
    ConferenceAdd {
        /// Conference ID
        conf_id: String,
        /// Leg to add
        leg_id: LegId,
    },

    /// Remove a leg from a conference
    ConferenceRemove {
        /// Conference ID
        conf_id: String,
        /// Leg to remove
        leg_id: LegId,
    },

    /// Mute a leg in a conference
    ConferenceMute {
        /// Conference ID
        conf_id: String,
        /// Leg to mute
        leg_id: LegId,
    },

    /// Unmute a leg in a conference
    ConferenceUnmute {
        /// Conference ID
        conf_id: String,
        /// Leg to unmute
        leg_id: LegId,
    },

    /// Destroy a conference
    ConferenceDestroy {
        /// Conference ID
        conf_id: String,
    },

    /// Enqueue a leg into a queue
    QueueEnqueue {
        /// Leg to enqueue
        leg_id: LegId,
        /// Queue ID or name
        queue_id: String,
        /// Priority (higher = more important)
        priority: Option<u32>,
    },

    /// Remove a leg from a queue
    QueueDequeue {
        /// Leg to dequeue
        leg_id: LegId,
    },

    /// Start an application (IVR, Voicemail, etc.)
    StartApp {
        /// Application name
        app_name: String,
        /// Application parameters
        params: Option<serde_json::Value>,
        /// Whether to auto-answer the call
        auto_answer: bool,
    },

    /// Stop the current application
    StopApp {
        /// Reason for stopping
        reason: Option<String>,
    },

    /// Inject an event into the running application
    InjectAppEvent {
        /// The event to inject
        event: AppEvent,
    },

    /// Handle a re-INVITE
    HandleReInvite {
        /// Target leg
        leg_id: LegId,
        /// New SDP
        sdp: String,
    },

    /// Refresh the session (send re-INVITE)
    RefreshSession,

    /// Mute a specific track
    MuteTrack {
        /// Track ID
        track_id: String,
    },

    /// Unmute a specific track
    UnmuteTrack {
        /// Track ID
        track_id: String,
    },

    /// Send a SIP MESSAGE request
    SendSipMessage {
        /// Content-Type header value
        content_type: String,
        /// Message body
        body: String,
    },

    /// Send a SIP NOTIFY request
    SendSipNotify {
        /// Event header value
        event: String,
        /// Content-Type header value
        content_type: String,
        /// Notify body
        body: String,
    },

    /// Join a conference mixer (for attended-transfer or 3-way calling)
    JoinMixer {
        /// Mixer ID / conference room ID
        mixer_id: String,
    },

    /// Leave the current conference mixer
    LeaveMixer,

    /// Send a SIP OPTIONS ping
    SendSipOptionsPing,

    /// Add a new SIP leg to the session
    LegAdd {
        /// SIP URI target
        target: String,
        /// Optional leg ID (auto-generated if not provided)
        leg_id: Option<LegId>,
    },

    /// Remove a leg from the session
    LegRemove {
        /// Leg ID to remove
        leg_id: LegId,
    },

    /// Leg dial completed successfully (async notification)
    LegConnected {
        /// Leg ID that connected
        leg_id: LegId,
    },

    /// Leg dial failed (async notification)
    LegFailed {
        /// Leg ID that failed
        leg_id: LegId,
        /// Failure reason
        reason: String,
    },
}

/// Point-to-point bridge mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum P2PMode {
    /// Standard audio bridge
    #[default]
    Audio,
    /// Video bridge
    Video,
    /// Audio and video
    AudioVideo,
}

/// Audio playback options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayOptions {
    /// Whether to loop the audio
    pub loop_playback: bool,
    /// Whether to wait for completion before returning
    pub await_completion: bool,
    /// Whether to interrupt on DTMF
    pub interrupt_on_dtmf: bool,
    /// Optional track ID for tracking
    pub track_id: Option<String>,
    /// Whether to send progress (183) before playing
    pub send_progress: bool,
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self {
            loop_playback: false,
            await_completion: false,
            interrupt_on_dtmf: true,
            track_id: None,
            send_progress: false,
        }
    }
}

/// Recording configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConfig {
    /// Output file path
    pub path: String,
    /// Maximum recording duration
    pub max_duration_secs: Option<u32>,
    /// Whether to play a beep before recording
    pub beep: bool,
    /// Audio format
    pub format: Option<String>,
}

/// Conference options
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConferenceOptions {
    /// Maximum number of participants
    pub max_participants: Option<u32>,
    /// Whether to record the conference
    pub record: bool,
    /// Recording path (if recording)
    pub record_path: Option<String>,
}

/// Application event for injection
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    /// DTMF digit received
    Dtmf { digit: String },
    /// Audio playback completed
    AudioComplete { track_id: String, interrupted: bool },
    /// Recording completed
    RecordingComplete { recording_id: String, path: String },
    /// Custom event
    Custom {
        name: String,
        data: serde_json::Value,
    },
    /// Timeout event
    Timeout { timer_id: String },
}

impl CallCommand {
    /// Check if this command requires media capabilities
    pub fn requires_media(&self) -> bool {
        matches!(
            self,
            CallCommand::Play { .. }
                | CallCommand::StartRecording { .. }
                | CallCommand::SupervisorListen { .. }
                | CallCommand::SupervisorWhisper { .. }
                | CallCommand::SupervisorBarge { .. }
                | CallCommand::SupervisorTakeover { .. }
                | CallCommand::Hold { music: Some(_), .. }
        )
    }

    /// Check if this is a signaling-only command (works in bypass mode)
    pub fn is_signaling_only(&self) -> bool {
        matches!(
            self,
            CallCommand::Answer { .. }
                | CallCommand::Reject { .. }
                | CallCommand::Hangup(_)
                | CallCommand::Transfer { .. }
                | CallCommand::Hold { music: None, .. }
                | CallCommand::Unhold { .. }
        )
    }

    /// Get the target leg ID if this command targets a specific leg
    pub fn target_leg(&self) -> Option<&LegId> {
        match self {
            CallCommand::Answer { leg_id } => Some(leg_id),
            CallCommand::Reject { leg_id, .. } => Some(leg_id),
            CallCommand::Ring { leg_id, .. } => Some(leg_id),
            CallCommand::Hangup(cmd) => cmd.leg_id.as_ref(),
            CallCommand::Bridge { leg_a, .. } => Some(leg_a),
            CallCommand::Unbridge { leg_id } => Some(leg_id),
            CallCommand::Transfer { leg_id, .. } => Some(leg_id),
            CallCommand::Hold { leg_id, .. } => Some(leg_id),
            CallCommand::Unhold { leg_id } => Some(leg_id),
            CallCommand::Play {
                leg_id: Some(leg_id),
                ..
            } => Some(leg_id),
            CallCommand::StopPlayback {
                leg_id: Some(leg_id),
            } => Some(leg_id),
            CallCommand::SendDtmf { leg_id, .. } => Some(leg_id),
            CallCommand::SupervisorListen { supervisor_leg, .. } => Some(supervisor_leg),
            CallCommand::SupervisorWhisper { supervisor_leg, .. } => Some(supervisor_leg),
            CallCommand::SupervisorBarge { supervisor_leg, .. } => Some(supervisor_leg),
            CallCommand::SupervisorTakeover { supervisor_leg, .. } => Some(supervisor_leg),
            CallCommand::SupervisorStop { supervisor_leg } => Some(supervisor_leg),
            CallCommand::ConferenceAdd { leg_id, .. } => Some(leg_id),
            CallCommand::ConferenceRemove { leg_id, .. } => Some(leg_id),
            CallCommand::ConferenceMute { leg_id, .. } => Some(leg_id),
            CallCommand::ConferenceUnmute { leg_id, .. } => Some(leg_id),
            CallCommand::QueueEnqueue { leg_id, .. } => Some(leg_id),
            CallCommand::QueueDequeue { leg_id } => Some(leg_id),
            CallCommand::HandleReInvite { leg_id, .. } => Some(leg_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_command_requires_media() {
        let play = CallCommand::Play {
            leg_id: None,
            source: MediaSource::file("test.wav"),
            options: None,
        };
        assert!(play.requires_media());

        let answer = CallCommand::Answer {
            leg_id: LegId::new("leg-1"),
        };
        assert!(!answer.requires_media());
    }

    #[test]
    fn call_command_signaling_only() {
        let answer = CallCommand::Answer {
            leg_id: LegId::new("leg-1"),
        };
        assert!(answer.is_signaling_only());

        let play = CallCommand::Play {
            leg_id: None,
            source: MediaSource::file("test.wav"),
            options: None,
        };
        assert!(!play.is_signaling_only());
    }

    #[test]
    fn call_command_target_leg() {
        let answer = CallCommand::Answer {
            leg_id: LegId::new("leg-1"),
        };
        assert_eq!(answer.target_leg().map(|l| l.as_str()), Some("leg-1"));

        let start_recording = CallCommand::StartRecording {
            config: RecordConfig {
                path: "/tmp/rec.wav".to_string(),
                max_duration_secs: None,
                beep: false,
                format: None,
            },
        };
        assert!(start_recording.target_leg().is_none());
    }
}
