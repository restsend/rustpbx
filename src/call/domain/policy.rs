//! Session policy types - controlling session behavior

use serde::{Deserialize, Serialize};

use super::{HangupCascade, MediaPathMode};

/// Policy controlling how a session behaves
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPolicy {
    /// How hangup cascades to other legs
    pub hangup_cascade: HangupCascade,
    /// Default media path mode for this session
    pub media_path: MediaPathMode,
    /// Whether to hangup all legs when the session ends
    pub hangup_all_on_end: bool,
    /// Whether to allow transfer
    pub allow_transfer: bool,
    /// Whether to allow hold
    pub allow_hold: bool,
    /// Whether to allow recording
    pub allow_recording: bool,
    /// Session timeout in seconds (0 = no timeout)
    pub timeout_secs: u32,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            hangup_cascade: HangupCascade::All,
            media_path: MediaPathMode::Anchored,
            hangup_all_on_end: true,
            allow_transfer: true,
            allow_hold: true,
            allow_recording: true,
            timeout_secs: 0,
        }
    }
}

impl SessionPolicy {
    /// Policy for inbound SIP calls
    pub fn inbound_sip() -> Self {
        Self {
            hangup_cascade: HangupCascade::All,
            hangup_all_on_end: true,
            ..Default::default()
        }
    }

    /// Policy for RWI-originated calls
    pub fn rwi_originate(auto_cascade: bool) -> Self {
        Self {
            hangup_cascade: if auto_cascade {
                HangupCascade::All
            } else {
                HangupCascade::None
            },
            hangup_all_on_end: auto_cascade,
            ..Default::default()
        }
    }

    /// Policy for consultation legs (during attended transfer)
    pub fn consultation() -> Self {
        Self {
            hangup_cascade: HangupCascade::None,
            hangup_all_on_end: false,
            allow_transfer: false,
            ..Default::default()
        }
    }

    /// Policy for supervisor legs
    pub fn supervisor() -> Self {
        Self {
            hangup_cascade: HangupCascade::None,
            hangup_all_on_end: false,
            allow_transfer: false,
            allow_recording: true,
            ..Default::default()
        }
    }

    /// Policy for conference calls
    pub fn conference() -> Self {
        Self {
            hangup_cascade: HangupCascade::None,
            hangup_all_on_end: false,
            allow_transfer: false,
            allow_hold: false,
            ..Default::default()
        }
    }

    /// Policy for extension-to-extension internal calls
    pub fn extension_internal() -> Self {
        Self {
            hangup_cascade: HangupCascade::All,
            hangup_all_on_end: true,
            allow_transfer: true,
            allow_hold: true,
            ..Default::default()
        }
    }

    /// Policy for call center queue calls
    pub fn call_center_queue() -> Self {
        Self {
            hangup_cascade: HangupCascade::All,
            hangup_all_on_end: true,
            allow_transfer: true,
            allow_hold: true,
            allow_recording: true,
            ..Default::default()
        }
    }

    /// Set the media path mode
    pub fn with_media_path(mut self, path: MediaPathMode) -> Self {
        self.media_path = path;
        self
    }

    /// Set whether transfer is allowed
    pub fn with_allow_transfer(mut self, allow: bool) -> Self {
        self.allow_transfer = allow;
        self
    }

    /// Set whether recording is allowed
    pub fn with_allow_recording(mut self, allow: bool) -> Self {
        self.allow_recording = allow;
        self
    }

    /// Set session timeout
    pub fn with_timeout(mut self, secs: u32) -> Self {
        self.timeout_secs = secs;
        self
    }
}

/// Ringback policy - how to handle ringback tone
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[derive(Default)]
pub enum RingbackPolicy {
    /// Pass through carrier early media
    #[default]
    PassThrough,
    /// Block early media, no ringback
    Block,
    /// Replace with local audio source
    Replace { source: MediaSource },
    /// Conditional: wait for remote, fallback to local on timeout
    Conditional {
        remote_timeout_ms: Option<u32>,
        fallback: MediaSource,
    },
}


/// Media source for playback
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    /// Play from file path
    File { path: String },
    /// Play from URL
    Url { url: String },
    /// Play text-to-speech
    Tts { text: String, voice: Option<String> },
    /// Play silence
    Silence,
    /// Play generated tone
    Tone { frequency: u32, duration_ms: u32 },
}

impl MediaSource {
    pub fn file(path: impl Into<String>) -> Self {
        Self::File { path: path.into() }
    }

    pub fn url(url: impl Into<String>) -> Self {
        Self::Url { url: url.into() }
    }

    pub fn tts(text: impl Into<String>) -> Self {
        Self::Tts {
            text: text.into(),
            voice: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_policy_inbound_sip() {
        let policy = SessionPolicy::inbound_sip();
        assert_eq!(policy.hangup_cascade, HangupCascade::All);
        assert!(policy.hangup_all_on_end);
    }

    #[test]
    fn session_policy_consultation() {
        let policy = SessionPolicy::consultation();
        assert_eq!(policy.hangup_cascade, HangupCascade::None);
        assert!(!policy.hangup_all_on_end);
        assert!(!policy.allow_transfer);
    }

    #[test]
    fn session_policy_rwi_originate() {
        let auto_cascade = SessionPolicy::rwi_originate(true);
        assert_eq!(auto_cascade.hangup_cascade, HangupCascade::All);

        let no_cascade = SessionPolicy::rwi_originate(false);
        assert_eq!(no_cascade.hangup_cascade, HangupCascade::None);
    }

    #[test]
    fn ringback_policy_default() {
        let policy = RingbackPolicy::default();
        assert!(matches!(policy, RingbackPolicy::PassThrough));
    }

    #[test]
    fn media_source_file() {
        let source = MediaSource::file("/path/to/audio.wav");
        assert!(matches!(source, MediaSource::File { .. }));
    }
}
