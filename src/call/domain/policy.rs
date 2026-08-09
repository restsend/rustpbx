//! Session media policy types

use serde::{Deserialize, Serialize};

/// Ringback policy - how to handle ringback tone
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[derive(Default)]
pub enum RingbackPolicy {
    /// Pass through carrier early media
    #[default]
    PassThrough,
    /// Proactive 183 Session Progress with bridge-generated early media
    EarlyMedia { source: MediaSource },
}

/// Media source for playback
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    /// Play from file path
    File { path: String },
    /// Play from URL
    Url { url: String },
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
