use crate::media::media_bridge::MediaBridge;
use tokio_util::sync::CancellationToken;

/// Which external (WebSocket) media bridge a handle belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalBridgeKind {
    /// TTS/voip bridge established by `connect_bridge`.
    Voip,
    /// Realtime (AI voice) bridge.
    Realtime,
}

/// Handle to an EXTERNAL (WebSocket) media bridge attached to one leg —
/// TTS/voip (`connect_bridge`) or realtime AI voice (`RealtimeStart`).
/// Deliberately distinct from `ConferenceBridgeHandle`: the conference
/// slot's `conf_id` gates `update_media_path()` and must never be set by
/// an external bridge (a past cause of one-way audio).
pub struct ExternalBridgeHandle {
    pub cancel: CancellationToken,
    pub kind: ExternalBridgeKind,
    /// Realtime only: an unexpected endpoint close hangs the call up.
    pub hangup_on_disconnect: bool,
}

pub struct MediaState {
    pub caller_offer: Option<String>,
    /// The caller's ORIGINAL INVITE offer SDP, stored verbatim at INVITE
    /// time and NEVER rewritten. Used for hold/unhold re-INVITE SDP so the
    /// WebRTC peer (browser) receives its own SDP back (Chrome's parser
    /// rejects rustrtc-generated re-offers). `caller_offer` may be
    /// overwritten during media-bridge negotiation with the PBX's processed
    /// version — this field preserves the peer's original bytes.
    pub raw_caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub callee_offer_cached_webrtc: Option<bool>,
    pub answer: Option<String>,
    pub early_media_sent: bool,
    pub callee_answer_sdp: Option<String>,
    pub bridge: Option<MediaBridge>,
    pub recording: crate::media::media_recorder::RecordingSession,
}

impl MediaState {
    pub fn new(caller_offer: Option<String>) -> Self {
        Self {
            caller_offer,
            raw_caller_offer: None,
            callee_offer: None,
            callee_offer_cached_webrtc: None,
            answer: None,
            early_media_sent: false,
            callee_answer_sdp: None,
            bridge: None,
            recording: Default::default(),
        }
    }
}
