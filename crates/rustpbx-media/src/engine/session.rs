//! Per-call media state owned by the MediaEngine.
//!
//! [`MediaSession`] is the engine's per-session state container.  It owns:
//!
//! * the negotiated [`BridgePeer`] (WebRTC↔RTP bridge or plain RTP bridge)
//! * active playback tracks keyed by `track_id`
//! * a [`McuSwitch`] for dynamic Bridge↔MCU transitions
//!
//! The session is created by [`MediaCommand::CreateSession`] and destroyed by
//! [`MediaSessionGuard`] (RAII).  The `BridgePeer` is injected later,
//! once SDP negotiation is complete, via [`MediaCommand::AttachBridge`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use tokio::sync::broadcast;

use crate::FileTrack;
use crate::bridge::{BridgeEndpoint, BridgePeer};
use crate::engine::event::MediaEvent;
use crate::engine::mcu_switch::McuSwitch;

/// Per-session media state owned by the engine.
pub struct MediaSession {
    pub session_id: String,

    // ── Bridge ──────────────────────────────────────────────────────────
    /// The active in-process media bridge, set once SDP negotiation is complete.
    pub bridge: Option<Arc<BridgePeer>>,

    /// Whether the SIP caller maps to `BridgeEndpoint::Caller` (`true`) or
    /// `BridgeEndpoint::Callee` (`false`).  Set together with the bridge.
    pub caller_is_webrtc: bool,

    /// Codec info from the caller-facing SDP answer.
    /// Used to configure FileTrack payload type for bridge playback.
    pub caller_codec_info: Vec<crate::negotiate::CodecInfo>,

    /// Codec info from the callee-facing SDP answer.
    /// Used to configure FileTrack playback on the callee endpoint when the
    /// callee uses a different codec than the caller (e.g. caller=Opus,
    /// callee=G.729).
    pub callee_codec_info: Vec<crate::negotiate::CodecInfo>,

    // ── Recording ──────────────────────────────────────────────────────
    /// Control handle for the lazily-created caller-facing recorder worker.
    pub recorder_control: Option<crate::recorder_tap::RecorderHandle>,

    // ── Playback ────────────────────────────────────────────────────────
    /// Active playback tracks keyed by caller-supplied `track_id`.
    pub playback_tracks: HashMap<String, FileTrack>,

    /// IDs of FileTrack(s) currently playing through the bridge, stored so
    /// StopPlayback can stop all of them (InjectAudio::Both creates two).
    pub bridge_playback_track_ids: Vec<String>,

    // ── MCU / TTS injection ─────────────────────────────────────────────
    /// Manages dynamic switching between direct bridge forwarding (low CPU)
    /// and MCU mixing (needed for TTS injection, conference).
    pub mcu: McuSwitch,
}

impl MediaSession {
    /// Create a new empty media session.
    pub fn new(session_id: String) -> Self {
        let mcu = McuSwitch::new(session_id.clone(), 8000);
        Self {
            session_id,
            bridge: None,
            caller_is_webrtc: false,
            caller_codec_info: vec![],
            callee_codec_info: vec![],
            recorder_control: None,
            playback_tracks: HashMap::new(),
            bridge_playback_track_ids: Vec::new(),
            mcu,
        }
    }

    /// Synchronously finalize all session resources.
    ///
    /// Called by [`MediaSessionGuard::drop`] — drains playback tracks (each
    /// `FileTrack::Drop` cancels + closes its PC), switches MCU back to bridge
    /// mode (drops the mixer, `ConferenceAudioMixer::Drop` aborts the mixing
    /// task), drops the bridge (`BridgePeer::Drop` calls `close_sync()`), and
    /// drops the recorder control handle so its worker drains and finalizes.
    pub fn finalize(&mut self) {
        self.playback_tracks.drain();
        self.bridge_playback_track_ids.clear();

        if self.mcu.mode() == crate::engine::mcu_switch::MediaMode::Mcu {
            self.mcu.switch_to_bridge();
        }

        self.bridge = None;
        self.recorder_control = None;
    }

    /// Resolve the bridge endpoint for the SIP caller leg.
    pub fn caller_endpoint(&self) -> BridgeEndpoint {
        if self.caller_is_webrtc {
            BridgeEndpoint::Caller
        } else {
            BridgeEndpoint::Callee
        }
    }

    /// Resolve the bridge endpoint for the SIP callee leg.
    pub fn callee_endpoint(&self) -> BridgeEndpoint {
        if self.caller_is_webrtc {
            BridgeEndpoint::Callee
        } else {
            BridgeEndpoint::Caller
        }
    }

    /// Map a `leg_id` string to the corresponding [`BridgeEndpoint`].
    /// Falls back to the caller endpoint for unknown IDs.
    pub fn endpoint_for_leg(&self, leg_id: &str) -> BridgeEndpoint {
        match leg_id {
            "callee" => self.callee_endpoint(),
            _ => self.caller_endpoint(),
        }
    }
}

// ---------------------------------------------------------------------------
// MediaSessionGuard — RAII eviction from the shared session map
// ---------------------------------------------------------------------------

/// An RAII guard that ties the [`MediaSession`] lifecycle to its owner.
///
/// When the guard is dropped, the session is synchronously removed from the
/// engine's shared session map and [`MediaSession::finalize`] is called
/// inline.  No command-channel communication is involved, so the leak-by-lost-
/// command failure mode cannot occur.
pub struct MediaSessionGuard {
    sessions: Weak<DashMap<String, MediaSession>>,
    event_tx: broadcast::Sender<MediaEvent>,
    session_id: String,
    finalized: AtomicBool,
}

impl MediaSessionGuard {
    pub fn new(
        sessions: Weak<DashMap<String, MediaSession>>,
        event_tx: broadcast::Sender<MediaEvent>,
        session_id: String,
    ) -> Self {
        Self {
            sessions,
            event_tx,
            session_id,
            finalized: AtomicBool::new(false),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for MediaSessionGuard {
    fn drop(&mut self) {
        if self.finalized.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(sessions) = self.sessions.upgrade() else {
            return;
        };
        if let Some((_, mut sess)) = sessions.remove(&self.session_id) {
            sess.finalize();
        }
        let _ = self.event_tx.send(MediaEvent::SessionDestroyed {
            session_id: self.session_id.clone(),
        });
    }
}

impl std::fmt::Debug for MediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSession")
            .field("session_id", &self.session_id)
            .field("bridge_present", &self.bridge.is_some())
            .field("caller_is_webrtc", &self.caller_is_webrtc)
            .field("recorder_attached", &self.recorder_control.is_some())
            .field("bridge_playback_track_ids", &self.bridge_playback_track_ids)
            .field(
                "playback_tracks",
                &self.playback_tracks.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}
