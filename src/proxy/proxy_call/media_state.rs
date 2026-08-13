use crate::media::media_bridge::MediaBridge;

pub struct MediaState {
    pub caller_offer: Option<String>,
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
            callee_offer: None,
            callee_offer_cached_webrtc: None,
            answer: None,
            early_media_sent: false,
            callee_answer_sdp: None,
            bridge: None,
        }
    }
}
