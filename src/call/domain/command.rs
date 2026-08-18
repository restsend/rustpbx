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
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::{HangupCommand, LegId, MediaSource, RingbackPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferOutcome {
    NotConnected,
    TargetEnded,
}

/// Type alias for CallCommand sender.
pub type CallCommandTx = mpsc::Sender<CallCommand>;
/// Type alias for CallCommand receiver.
pub type CallCommandRx = mpsc::Receiver<CallCommand>;

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

    TransferAwaitResult {
        leg_id: LegId,
        target: String,
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

    /// Start live transcription. Reference-counted: the transcription pump
    /// starts on the first request and runs until the matching number of
    /// `StopTranscription` commands (or call end).
    StartTranscription {
        /// Optional language override (provider-specific BCP-47 tag).
        language: Option<String>,
    },

    /// Stop one live-transcription reference (see `StartTranscription`).
    StopTranscription,

    /// Append an event to the session's call trace timeline.
    ///
    /// Used by call applications (voicemail, IVR, ...) to contribute
    /// diagnostic trace entries that end up in the call-record
    /// `metadata["trace"]` JSON array.
    Trace {
        /// The trace event to append.
        event: crate::call_errors::TraceEvent,
    },

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

    /// Join a specific leg of this session into a conference mixer.
    ///
    /// Used by consult-transfer merge to bridge exactly the right leg per
    /// session: session_a's "caller" leg (the customer A) and session_b's
    /// "callee" leg (the expert C). The plain `JoinMixer` variant
    /// hard-codes the callee leg, which would bridge B instead of A and
    /// leave the conference silent once B exits.
    JoinMixerLeg {
        /// Mixer ID / conference room ID
        mixer_id: String,
        /// Which leg of this session to bridge ("caller" or "callee").
        leg_id: LegId,
    },

    /// Join the caller leg into a conference room, waiting for the leg to be
    /// media-ready first (room dial-in via app=conference). Processed after
    /// any queued Answer command, so the caller leg is Connected by the time
    /// the join runs.
    JoinConference {
        /// Conference room ID
        conf_id: String,
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
    /// Leg connected (async notification)
    LegConnected {
        /// Leg ID that connected
        leg_id: LegId,
        /// Answer SDP from the leg (for codec resolution)
        answer_sdp: Option<String>,
        /// Call-ID of the connected dialog (used to attach a real dialog to
        /// the session's MediaBridge leg in UAC mode).
        dialog_id: Option<String>,
    },

    /// Leg received 180 Ringing (async notification from the dynamic-leg
    /// INVITE task). Lets the session fire `on_call_ringing` session hooks so
    /// the CC addon can emit `cc_ringing` for queue-dialed agents.
    LegRinging {
        /// Leg ID that is ringing
        leg_id: LegId,
    },

    /// Leg dial failed (async notification)
    LegFailed {
        /// Leg ID that failed
        leg_id: LegId,
        /// Failure reason
        reason: String,
    },

    /// Application exited (async notification sent by app runtime when the
    /// running CallApp event loop finishes, for any reason).
    AppExited,

    /// Start the post-disconnect return app (if any) after agent/B-leg hangs
    /// up.  The handler reads `meta.transfer_return_app` and dispatches via
    /// `ensure_app_running`.  CSAT hooks (`on_agent_disconnected`) take
    /// precedence over the stored return spec.
    StartReturnApp,

    /// Restore the MediaBridge route after an announcement/playback finished
    /// (re-activates fast-path relay or transcode). Issued internally when a
    /// playback handle's `done` resolves.
    ResumeMedia,

    /// Fast-path relay arming failed (e.g. a WebRTC leg's DTLS/SRTP transport
    /// never became ready). Fall back to transcoding so the call keeps media.
    /// Issued by the relay-arm-failure monitor spawned alongside the bridge.
    RelayArmFailure,

    /// Send a SIP INFO request to a specific dialog with a custom body and
    /// content-type.  Used by the IVR-exec flow to deliver the result back
    /// to the cc-phone agent.
    SendInfo {
        /// Dialog leg ID to send to (e.g. "callee").
        leg_id: LegId,
        /// Content-Type header value.
        content_type: String,
        /// Body bytes.
        body: Vec<u8>,
    },
}

/// Generic descriptor for an app to start as the "return" destination after a
/// connected B-leg (agent / bridge) disconnects.
///
/// Stored in [`crate::proxy::proxy_call::call_meta::CallMeta`] and consumed by
/// the `CallCommand::StartReturnApp` handler.  Structurally identical to
/// `CallCommand::StartApp` minus `auto_answer` — `(app_name, params)` is the
/// lingua franca for app construction throughout the codebase.
#[derive(Debug, Clone)]
pub struct ReturnAppSpec {
    /// Application name, e.g. `"ivr"`, `"voicemail"`, `"queue"`, `"csat_survey"`.
    pub app_name: String,
    /// Application-specific parameters (same shape as `CallCommand::StartApp::params`).
    pub params: serde_json::Value,
}

impl ReturnAppSpec {
    /// Build an IVR return spec from an IVR file name and optional extra params
    /// (e.g. `return_menu`, `return_step_id`).
    pub fn ivr(ivr_file: impl Into<String>, extra_params: HashMap<String, String>) -> Self {
        let mut app_params = serde_json::json!({"file": ivr_file.into()});
        if !extra_params.is_empty() {
            app_params["ivr_params"] = serde_json::json!(extra_params);
        }
        Self {
            app_name: "ivr".to_string(),
            params: app_params,
        }
    }
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
    /// Play on the target leg only, without mirroring the announcement onto
    /// the opposite leg (caller-exclusive prompts).
    #[serde(default)]
    pub side_only: bool,
}

impl Default for PlayOptions {
    fn default() -> Self {
        Self {
            loop_playback: false,
            await_completion: false,
            interrupt_on_dtmf: true,
            track_id: None,
            send_progress: false,
            side_only: false,
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
    /// Output channel count. `None`/`2` = stereo (both legs interleaved),
    /// `Some(1)` = mono.
    #[serde(default)]
    pub channels: Option<u16>,
    /// When `Some(true)` and `channels == Some(1)`, write only the caller's
    /// ingress (leg A) into the mono output at full amplitude. Used by
    /// voicemail, where the egress leg is silence during the message.
    #[serde(default)]
    pub mono_caller_only: Option<bool>,
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
                | CallCommand::StartTranscription { .. }
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
                | CallCommand::TransferAwaitResult { .. }
                | CallCommand::Hold { music: None, .. }
                | CallCommand::Unhold { .. }
                | CallCommand::Trace { .. }
        )
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
}
