use crate::media::media_bridge::MediaBridge;

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
        }
    }
}
