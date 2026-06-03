use crate::media::FileTrack;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct CallerIngressMonitor {
    pub cancel_token: CancellationToken,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub enum RecordingPhase {
    Idle,
    Recording {
        path: String,
        started_at: Instant,
        max_duration: Option<Duration>,
    },
    Paused {
        path: String,
        started_at: Instant,
        max_duration: Option<Duration>,
    },
}

impl RecordingPhase {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            RecordingPhase::Recording { .. } | RecordingPhase::Paused { .. }
        )
    }

    #[allow(dead_code)]
    pub fn is_recording(&self) -> bool {
        matches!(self, RecordingPhase::Recording { .. })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> Option<&str> {
        match self {
            RecordingPhase::Recording { path, .. } | RecordingPhase::Paused { path, .. } => {
                Some(path)
            }
            _ => None,
        }
    }

    pub fn started_at(&self) -> Option<Instant> {
        match self {
            RecordingPhase::Recording { started_at, .. }
            | RecordingPhase::Paused { started_at, .. } => Some(*started_at),
            _ => None,
        }
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.started_at().map(|t| t.elapsed())
    }

    #[allow(dead_code)]
    pub fn check_max_duration(&self) -> bool {
        if let RecordingPhase::Recording {
            started_at,
            max_duration: Some(max),
            ..
        } = self
        {
            started_at.elapsed() >= *max
        } else {
            false
        }
    }
}

pub struct MediaState {
    pub caller_offer: Option<String>,
    pub callee_offer: Option<String>,
    pub answer: Option<String>,
    pub early_media_sent: bool,
    pub callee_answer_sdp: Option<String>,
    pub recording_state: RecordingPhase,
    pub playback_tracks: HashMap<String, FileTrack>,
    pub caller_ingress_monitor: Option<CallerIngressMonitor>,
    pub media_bridge: Option<Arc<crate::media::bridge::BridgePeer>>,
    pub caller_answer_uses_media_bridge: bool,
    pub callee_offer_uses_media_bridge: bool,
    pub media_bridge_started: bool,
    pub bridge_playback_track_id: Option<String>,
    pub rtp_timeout_tx: Option<mpsc::Sender<String>>,
}

impl MediaState {
    pub fn new(caller_offer: Option<String>) -> Self {
        Self {
            caller_offer,
            callee_offer: None,
            answer: None,
            early_media_sent: false,
            callee_answer_sdp: None,
            recording_state: RecordingPhase::Idle,
            playback_tracks: HashMap::new(),
            caller_ingress_monitor: None,
            media_bridge: None,
            caller_answer_uses_media_bridge: false,
            callee_offer_uses_media_bridge: false,
            media_bridge_started: false,
            bridge_playback_track_id: None,
            rtp_timeout_tx: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_recording_phase_idle_default() {
        let state = RecordingPhase::Idle;
        assert!(!state.is_active());
        assert!(!state.is_recording());
        assert!(state.path().is_none());
        assert!(state.started_at().is_none());
        assert!(!state.check_max_duration());
    }

    #[test]
    fn test_recording_phase_recording() {
        let state = RecordingPhase::Recording {
            path: "/tmp/test.wav".to_string(),
            started_at: Instant::now(),
            max_duration: Some(Duration::from_secs(30)),
        };
        assert!(state.is_active());
        assert!(state.is_recording());
        assert_eq!(state.path(), Some("/tmp/test.wav"));
        assert!(state.started_at().is_some());
        assert!(!state.check_max_duration());
    }

    #[test]
    fn test_recording_phase_paused() {
        let state = RecordingPhase::Paused {
            path: "/tmp/test.wav".to_string(),
            started_at: Instant::now(),
            max_duration: None,
        };
        assert!(state.is_active());
        assert!(!state.is_recording());
        assert_eq!(state.path(), Some("/tmp/test.wav"));
    }

    #[test]
    fn test_recording_phase_max_duration_not_expired() {
        let state = RecordingPhase::Recording {
            path: "/tmp/test.wav".to_string(),
            started_at: Instant::now(),
            max_duration: Some(Duration::from_secs(3600)),
        };
        assert!(!state.check_max_duration());
    }

    #[test]
    fn test_recording_phase_no_max_duration() {
        let state = RecordingPhase::Recording {
            path: "/tmp/test.wav".to_string(),
            started_at: Instant::now(),
            max_duration: None,
        };
        assert!(!state.check_max_duration());
    }

    #[test]
    fn test_recording_phase_elapsed() {
        let state = RecordingPhase::Recording {
            path: "/tmp/test.wav".to_string(),
            started_at: Instant::now(),
            max_duration: None,
        };
        let elapsed = state.elapsed().unwrap();
        assert!(elapsed < Duration::from_millis(100));
    }
}
