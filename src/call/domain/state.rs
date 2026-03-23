//! Session state and media path types

use serde::{Deserialize, Serialize};

/// Overall state of a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Session is being initialized
    Initializing,
    /// Session is ringing (at least one leg ringing)
    Ringing,
    /// Early media is active
    EarlyMedia,
    /// Session is active (legs are bridged)
    Active,
    /// Session is on hold
    Held,
    /// Transfer in progress
    Transferring,
    /// Application is running (IVR, Voicemail, etc.)
    AppRunning,
    /// Session is being terminated
    Ending,
    /// Session has been terminated
    Ended,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::Initializing
    }
}

impl From<crate::proxy::proxy_call::state::ProxyCallPhase> for SessionState {
    fn from(phase: crate::proxy::proxy_call::state::ProxyCallPhase) -> Self {
        use crate::proxy::proxy_call::state::ProxyCallPhase;
        match phase {
            ProxyCallPhase::Initializing => SessionState::Initializing,
            ProxyCallPhase::Ringing => SessionState::Ringing,
            ProxyCallPhase::EarlyMedia => SessionState::EarlyMedia,
            ProxyCallPhase::Bridged => SessionState::Active,
            ProxyCallPhase::Terminating => SessionState::Ending,
            ProxyCallPhase::Failed => SessionState::Ended,
            ProxyCallPhase::Ended => SessionState::Ended,
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Initializing => write!(f, "initializing"),
            SessionState::Ringing => write!(f, "ringing"),
            SessionState::EarlyMedia => write!(f, "early_media"),
            SessionState::Active => write!(f, "active"),
            SessionState::Held => write!(f, "held"),
            SessionState::Transferring => write!(f, "transferring"),
            SessionState::AppRunning => write!(f, "app_running"),
            SessionState::Ending => write!(f, "ending"),
            SessionState::Ended => write!(f, "ended"),
        }
    }
}

/// Media path mode - how RTP flows through the system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaPathMode {
    /// RTP is anchored at PBX/media-proxy; PBX can observe/manipulate media directly.
    /// Required for: recording, DTMF detection, supervisor modes, media injection.
    Anchored,
    /// Endpoint-to-endpoint RTP bypass; PBX keeps signaling/control only.
    /// PBX cannot record, inject media, or perform supervisor operations.
    Bypass,
    /// Policy decides per leg/bridge at runtime based on capabilities and requirements.
    Adaptive,
}

impl Default for MediaPathMode {
    fn default() -> Self {
        Self::Anchored
    }
}

impl std::fmt::Display for MediaPathMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaPathMode::Anchored => write!(f, "anchored"),
            MediaPathMode::Bypass => write!(f, "bypass"),
            MediaPathMode::Adaptive => write!(f, "adaptive"),
        }
    }
}

/// Media capabilities based on the current media path mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCapability {
    /// Full media capabilities: play, record, listen, inject, transcode
    Full,
    /// Limited capabilities (e.g., only DTMF events)
    Limited,
    /// Signaling only - no media processing possible
    SignalingOnly,
}

impl MediaCapability {
    /// Derive capability from media path mode
    pub fn from_media_path(mode: MediaPathMode) -> Self {
        match mode {
            MediaPathMode::Anchored => MediaCapability::Full,
            MediaPathMode::Bypass => MediaCapability::SignalingOnly,
            MediaPathMode::Adaptive => MediaCapability::Limited, // Default to limited for adaptive
        }
    }
}

/// Runtime profile for media operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRuntimeProfile {
    /// Current media path mode
    pub path: MediaPathMode,
    /// Current media capabilities
    pub capability: MediaCapability,
    /// Whether local ringback is supported
    pub supports_local_ringback: bool,
    /// Whether recording is supported
    pub supports_recording: bool,
    /// Whether supervisor media operations (listen/whisper/barge) are supported
    pub supports_supervisor_media: bool,
    /// Whether media injection is supported
    pub supports_media_injection: bool,
}

impl Default for MediaRuntimeProfile {
    fn default() -> Self {
        Self::from_media_path(MediaPathMode::Anchored)
    }
}

impl MediaRuntimeProfile {
    pub fn from_media_path(path: MediaPathMode) -> Self {
        let capability = MediaCapability::from_media_path(path);
        let full = capability == MediaCapability::Full;

        Self {
            path,
            capability,
            supports_local_ringback: full,
            supports_recording: full,
            supports_supervisor_media: full,
            supports_media_injection: full,
        }
    }

    /// Check if a specific media operation is supported
    pub fn can_play(&self) -> bool {
        self.supports_local_ringback || self.capability != MediaCapability::SignalingOnly
    }

    pub fn can_record(&self) -> bool {
        self.supports_recording
    }

    pub fn can_supervise(&self) -> bool {
        self.supports_supervisor_media
    }

    pub fn can_inject(&self) -> bool {
        self.supports_media_injection
    }

    /// Create a degraded profile (for bypass mode)
    pub fn degraded() -> Self {
        Self::from_media_path(MediaPathMode::Bypass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_capability_from_path() {
        assert_eq!(
            MediaCapability::from_media_path(MediaPathMode::Anchored),
            MediaCapability::Full
        );
        assert_eq!(
            MediaCapability::from_media_path(MediaPathMode::Bypass),
            MediaCapability::SignalingOnly
        );
    }

    #[test]
    fn media_runtime_profile_anchored() {
        let profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored);
        assert!(profile.can_record());
        assert!(profile.can_supervise());
        assert!(profile.can_inject());
    }

    #[test]
    fn media_runtime_profile_bypass() {
        let profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);
        assert!(!profile.can_record());
        assert!(!profile.can_supervise());
        assert!(!profile.can_inject());
    }
}
