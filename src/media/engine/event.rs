//! MediaEngine Event types
//!
//! Events are emitted by the engine via `tokio::sync::broadcast` channel.
//! Subscribers can filter by session_id or event variant.

use serde::Serialize;

/// Recording result returned when recording stops.
#[derive(Debug, Clone, Serialize)]
pub struct RecordResult {
    pub path: String,
    pub duration_secs: f64,
    pub file_size: u64,
}

/// All events emitted by the MediaEngine.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaEvent {
    // ── Session ──────────────────────────────────────────────────────────
    SessionCreated { session_id: String },
    SessionDestroyed { session_id: String },

    // ── Leg ──────────────────────────────────────────────────────────────
    LegAdded {
        session_id: String,
        leg_id: String,
    },
    LegRemoved {
        session_id: String,
        leg_id: String,
    },

    // ── Bridge ───────────────────────────────────────────────────────────
    BridgeEstablished {
        session_id: String,
        leg_a: String,
        leg_b: String,
    },
    BridgeBroken {
        session_id: String,
        reason: String,
    },

    // ── Playback ─────────────────────────────────────────────────────────
    PlayStarted {
        session_id: String,
        leg_id: String,
        play_id: String,
    },
    PlayFinished {
        session_id: String,
        leg_id: String,
        play_id: String,
        interrupted: bool,
    },

    // ── Recording ────────────────────────────────────────────────────────
    RecordingStarted { session_id: String },
    RecordingStopped {
        session_id: String,
        result: RecordResult,
    },
    RecordingPaused { session_id: String },
    RecordingResumed { session_id: String },
    SipFlowStarted { session_id: String },
    SipFlowStopped { session_id: String },

    // ── DTMF ─────────────────────────────────────────────────────────────
    DtmfCollected {
        session_id: String,
        leg_id: String,
        digits: String,
    },

    // ── MCU / Conference ─────────────────────────────────────────────────
    MixerJoined {
        session_id: String,
        mixer_id: String,
    },
    MixerLeft {
        session_id: String,
        mixer_id: String,
    },

    // ── Hold ─────────────────────────────────────────────────────────────
    LegHeld {
        session_id: String,
        leg_id: String,
    },
    LegUnheld {
        session_id: String,
        leg_id: String,
    },

    // ── Errors ───────────────────────────────────────────────────────────
    Error {
        session_id: String,
        command: String,
        error: String,
    },
}

impl MediaEvent {
    /// Extract the session_id this event belongs to, if applicable.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::SessionCreated { session_id }
            | Self::SessionDestroyed { session_id } => Some(session_id),
            Self::LegAdded { session_id, .. }
            | Self::LegRemoved { session_id, .. } => Some(session_id),
            Self::BridgeEstablished { session_id, .. }
            | Self::BridgeBroken { session_id, .. } => Some(session_id),
            Self::PlayStarted { session_id, .. }
            | Self::PlayFinished { session_id, .. } => Some(session_id),
            Self::RecordingStarted { session_id }
            | Self::RecordingStopped { session_id, .. }
            | Self::RecordingPaused { session_id }
            | Self::RecordingResumed { session_id }
            | Self::SipFlowStarted { session_id }
            | Self::SipFlowStopped { session_id } => Some(session_id),
            Self::DtmfCollected { session_id, .. } => Some(session_id),
            Self::MixerJoined { session_id, .. }
            | Self::MixerLeft { session_id, .. } => Some(session_id),
            Self::LegHeld { session_id, .. }
            | Self::LegUnheld { session_id, .. } => Some(session_id),
            Self::Error { session_id, .. } => Some(session_id),
        }
    }
}
