//! MediaEngine Command types
//!
//! All operations on the MediaEngine are expressed as variants of [`MediaCommand`].
//! Commands are sent via `mpsc::Sender<MediaCommand>` to the engine's command loop.

use std::sync::Arc;

use audio_codec::CodecType;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// PcmFrame (used by PcmStream transport)
// ---------------------------------------------------------------------------

/// A raw PCM audio frame (16-bit signed, mono).
#[derive(Debug, Clone)]
pub struct PcmFrame {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// LegTransport
// ---------------------------------------------------------------------------

/// Describes how a leg connects to the media engine.
///
/// The engine itself is transport-agnostic: it works with audio samples
/// (either RTP-level `MediaSample` or decoded PCM).  The transport adapter
/// is responsible for bridging the underlying protocol to the engine's
/// expectations.
pub enum LegTransport {
    /// WebRTC peer connection (DTLS/SRTP, ICE).
    /// The engine will use the existing `PeerConnection` for RTP send/recv.
    Webrtc {
        peer_connection: rustrtc::PeerConnection,
    },
    /// Plain RTP peer connection (no DTLS/ICE).
    Rtp {
        peer_connection: rustrtc::PeerConnection,
    },
    /// A local audio file used as a leg (one-way: file → engine).
    /// Used for IVR prompts, hold music, or test harnesses.
    File { path: String },
    /// WebSocket media stream.
    /// Audio is exchanged as raw PCM16 frames over WebSocket.
    WebSocket {
        url: String,
        codec: CodecType,
        sample_rate: u32,
    },
    /// In-process PCM stream.
    /// Used for testing, TTS injection, and ASR integration.
    PcmStream {
        input: mpsc::Receiver<PcmFrame>,
        output: mpsc::Sender<PcmFrame>,
        sample_rate: u32,
    },
}

impl std::fmt::Display for LegTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Webrtc { .. } => write!(f, "webrtc"),
            Self::Rtp { .. } => write!(f, "rtp"),
            Self::File { path } => write!(f, "file({})", path),
            Self::WebSocket { url, .. } => write!(f, "ws({})", url),
            Self::PcmStream { .. } => write!(f, "pcm"),
        }
    }
}

// ---------------------------------------------------------------------------
// PlaySource / PlayOptions / InjectTarget
// ---------------------------------------------------------------------------

/// Source for audio playback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PlaySource {
    /// Local file path (WAV, raw, etc.).
    File { path: String },
    /// HTTP/HTTPS URL.
    Url { url: String },
    /// TTS synthesis request.
    Tts { text: String, voice: String },
    /// Silence generator.
    Silence,
}

/// Playback options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayOptions {
    /// Loop the audio until explicitly stopped.
    pub loop_playback: bool,
    /// Block until playback finishes before returning.
    pub await_completion: bool,
    /// Interrupt playback when DTMF is detected.
    pub interrupt_on_dtmf: bool,
    /// Optional caller-supplied track ID.
    pub track_id: Option<String>,
    /// Broadcast mode: both legs hear the audio, and peer-to-peer audio
    /// is suppressed during playback.
    pub broadcast_to_all: bool,
}

/// Target for audio injection (TTS / announcement).
#[derive(Debug, Clone)]
pub enum InjectTarget {
    /// Both legs hear the audio.
    Both,
    /// Only the specified leg hears the audio.
    Leg(String),
}

// ---------------------------------------------------------------------------
// CodecProfile / RecordConfig
// ---------------------------------------------------------------------------

/// Negotiated codec parameters for a leg.
#[derive(Debug, Clone)]
pub struct CodecProfile {
    pub codec: CodecType,
    pub payload_type: u8,
    pub clock_rate: u32,
}

impl CodecProfile {
    pub fn pcmu() -> Self {
        Self {
            codec: CodecType::PCMU,
            payload_type: 0,
            clock_rate: 8000,
        }
    }
}

/// Recording configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordConfig {
    pub path: String,
    pub max_duration_secs: Option<u32>,
    pub beep: bool,
    pub format: Option<String>,
    #[serde(default)]
    pub stereo_swap: bool,
}

// ---------------------------------------------------------------------------
// MediaCommand
// ---------------------------------------------------------------------------

/// The unified command enum for the MediaEngine.
///
/// All signaling adapters (SIP, WebSocket, HTTP, IVR apps) translate their
/// domain-specific operations into `MediaCommand` variants.
pub enum MediaCommand {
    // ── Session lifecycle ────────────────────────────────────────────────
    /// Create a session in the engine's session map.
    ///
    /// Prefer [`MediaEngine::create_session_guarded`] for new code — it
    /// returns an RAII [`MediaSessionGuard`] whose `Drop` synchronously
    /// removes the session from the map and finalizes resources, avoiding
    /// the lost-command leak that `DestroySession` was vulnerable to.
    CreateSession {
        session_id: String,
    },

    // ── Leg management ───────────────────────────────────────────────────
    AddLeg {
        session_id: String,
        leg_id: String,
        transport: LegTransport,
        codec_profile: Option<CodecProfile>,
    },
    RemoveLeg {
        session_id: String,
        leg_id: String,
    },

    // ── Bridge ───────────────────────────────────────────────────────────
    BridgeLegs {
        session_id: String,
        leg_a: String,
        leg_b: String,
    },
    Unbridge {
        session_id: String,
    },

    // ── Playback ─────────────────────────────────────────────────────────
    Play {
        session_id: String,
        leg_id: Option<String>,
        source: PlaySource,
        options: PlayOptions,
    },
    StopPlayback {
        session_id: String,
        leg_id: Option<String>,
    },

    // ── Recording ───────────────────────────────────────────────────────
    StartRecording {
        session_id: String,
        config: RecordConfig,
        /// The caller-facing profile is used for both captured directions.
        caller_profile: Option<crate::negotiate::NegotiatedLegProfile>,
        /// Retained for API compatibility; caller-facing recording does not use it.
        callee_profile: Option<crate::negotiate::NegotiatedLegProfile>,
        /// Confirms that the recording task accepted and created the recorder.
        reply: Option<tokio::sync::oneshot::Sender<()>>,
    },
    StopRecording {
        session_id: String,
        /// Returns after the file recorder has been finalized.
        reply: Option<tokio::sync::oneshot::Sender<crate::engine::event::RecordResult>>,
    },
    PauseRecording {
        session_id: String,
    },
    ResumeRecording {
        session_id: String,
    },

    SendDtmf {
        session_id: String,
        leg_id: String,
        digits: String,
    },
    CollectDtmf {
        session_id: String,
        leg_id: Option<String>,
        min_digits: u32,
        max_digits: u32,
        timeout_ms: u64,
        terminator: Option<char>,
    },

    // ── MCU / Conference ─────────────────────────────────────────────────
    JoinMixer {
        session_id: String,
        mixer_id: String,
    },
    LeaveMixer {
        session_id: String,
    },
    SetRouteGain {
        mixer_id: String,
        src_leg: String,
        dst_leg: String,
        gain: f32,
    },
    InjectAudio {
        session_id: String,
        source: PlaySource,
        target: InjectTarget,
        mute_peer: bool,
    },

    // ── Hold ─────────────────────────────────────────────────────────────
    Hold {
        session_id: String,
        leg_id: String,
        music: Option<PlaySource>,
    },
    Unhold {
        session_id: String,
        leg_id: String,
    },

    // ── Mute ─────────────────────────────────────────────────────────────
    MuteLeg {
        session_id: String,
        leg_id: String,
    },
    UnmuteLeg {
        session_id: String,
        leg_id: String,
    },

    // ── Ownership transfer from sip_session to engine ────────────────────
    /// Hand an already-negotiated BridgePeer to the engine.
    ///
    /// Called by sip_session after SDP offer/answer negotiation is complete.
    /// `caller_is_webrtc` — which BridgePeer side (Caller / Callee) maps to the
    /// SIP caller leg; the engine uses this to route Play/DTMF correctly.
    /// `caller_codec_info` — codec info from the caller-facing answer SDP,
    /// used to configure FileTrack payload type for playback.
    AttachBridge {
        session_id: String,
        bridge: Arc<crate::bridge::BridgePeer>,
        /// `true` → caller maps to BridgeEndpoint::Caller, `false` → Callee.
        caller_is_webrtc: bool,
        /// Codec info from the caller-facing SDP answer (for FileTrack playback).
        caller_codec_info: Vec<crate::negotiate::CodecInfo>,
        /// Codec info from the callee-facing SDP answer.
        callee_codec_info: Vec<crate::negotiate::CodecInfo>,
    },

    /// Remove and stop the current bridge for a session.
    DetachBridge {
        session_id: String,
    },
}

impl MediaCommand {
    /// Extract the session_id this command targets, if any.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::CreateSession { session_id } => Some(session_id),
            Self::AddLeg { session_id, .. }
            | Self::RemoveLeg { session_id, .. }
            | Self::BridgeLegs { session_id, .. }
            | Self::Unbridge { session_id } => Some(session_id),
            Self::Play { session_id, .. } | Self::StopPlayback { session_id, .. } => {
                Some(session_id)
            }
            Self::StartRecording { session_id, .. }
            | Self::PauseRecording { session_id }
            | Self::ResumeRecording { session_id }
            | Self::StopRecording { session_id, .. } => Some(session_id),
            Self::SendDtmf { session_id, .. } | Self::CollectDtmf { session_id, .. } => {
                Some(session_id)
            }
            Self::JoinMixer { session_id, .. } | Self::LeaveMixer { session_id } => {
                Some(session_id)
            }
            Self::InjectAudio { session_id, .. } => Some(session_id),
            Self::Hold { session_id, .. } | Self::Unhold { session_id, .. } => Some(session_id),
            Self::MuteLeg { session_id, .. } | Self::UnmuteLeg { session_id, .. } => {
                Some(session_id)
            }
            Self::AttachBridge { session_id, .. } | Self::DetachBridge { session_id } => {
                Some(session_id)
            }
            // Commands that don't target a single session
            Self::SetRouteGain { .. } => None,
        }
    }

    /// Human-readable command name for logging.
    pub fn name(&self) -> &'static str {
        match self {
            Self::CreateSession { .. } => "create_session",
            Self::AddLeg { .. } => "add_leg",
            Self::RemoveLeg { .. } => "remove_leg",
            Self::BridgeLegs { .. } => "bridge_legs",
            Self::Unbridge { .. } => "unbridge",
            Self::Play { .. } => "play",
            Self::StopPlayback { .. } => "stop_playback",
            Self::StartRecording { .. } => "start_recording",
            Self::PauseRecording { .. } => "pause_recording",
            Self::ResumeRecording { .. } => "resume_recording",
            Self::StopRecording { .. } => "stop_recording",
            Self::SendDtmf { .. } => "send_dtmf",
            Self::CollectDtmf { .. } => "collect_dtmf",
            Self::JoinMixer { .. } => "join_mixer",
            Self::LeaveMixer { .. } => "leave_mixer",
            Self::SetRouteGain { .. } => "set_route_gain",
            Self::InjectAudio { .. } => "inject_audio",
            Self::Hold { .. } => "hold",
            Self::Unhold { .. } => "unhold",
            Self::MuteLeg { .. } => "mute_leg",
            Self::UnmuteLeg { .. } => "unmute_leg",
            Self::AttachBridge { .. } => "attach_bridge",
            Self::DetachBridge { .. } => "detach_bridge",
        }
    }
}

impl std::fmt::Debug for MediaCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaCommand::")?;
        f.write_str(self.name())
    }
}
