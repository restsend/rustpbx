use crate::media::FileTrack;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) struct CallerIngressMonitor {
    pub cancel_token: CancellationToken,
    pub task: JoinHandle<()>,
}

pub struct MediaState {
    pub caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub answer: Option<String>,
    pub early_media_sent: bool,
    pub callee_answer_sdp: Option<String>,
    pub recording_state: Option<(String, Instant)>,
    pub playback_tracks: HashMap<String, FileTrack>,
    pub caller_ingress_monitor: Option<CallerIngressMonitor>,
    pub media_bridge: Option<Arc<crate::media::bridge::BridgePeer>>,
    pub caller_answer_uses_media_bridge: bool,
    pub callee_offer_uses_media_bridge: bool,
    pub media_bridge_started: bool,
    pub bridge_playback_track_id: Option<String>,
}

impl MediaState {
    pub fn new(caller_offer: Option<String>) -> Self {
        Self {
            caller_offer,
            callee_offer: None,
            answer: None,
            early_media_sent: false,
            callee_answer_sdp: None,
            recording_state: None,
            playback_tracks: HashMap::new(),
            caller_ingress_monitor: None,
            media_bridge: None,
            caller_answer_uses_media_bridge: false,
            callee_offer_uses_media_bridge: false,
            media_bridge_started: false,
            bridge_playback_track_id: None,
        }
    }
}
