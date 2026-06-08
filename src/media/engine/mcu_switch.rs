//! Dynamic Bridge ↔ MCU switching for TTS injection.
//!
//! Normal 2-party calls use `BridgePeer` (zero-copy RTP forwarding, ~0 CPU).
//! When TTS injection is needed, we switch to MCU mode:
//!
//! 1. Stop the BridgePeer
//! 2. Create a ConferenceAudioMixer
//! 3. Wire both legs through ConferenceMediaBridge (forward + reverse loops)
//! 4. Add a synthetic TTS participant, feed PCM
//! 5. When TTS is done, optionally switch back to bridge mode

use std::sync::Arc;

use anyhow::Result;
use tracing::info;

use crate::media::conference_mixer::ConferenceAudioMixer;

/// State machine for the media mode of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    /// Direct RTP forwarding via BridgePeer (zero-copy, minimal CPU).
    Bridge,
    /// MCU mixing via ConferenceAudioMixer (needed for TTS/conference).
    Mcu,
}

/// Manages the dynamic switching between bridge and MCU mode for a session.
pub struct McuSwitch {
    session_id: String,
    mixer: Option<Arc<ConferenceAudioMixer>>,
    mode: MediaMode,
    sample_rate: u32,
}

impl McuSwitch {
    pub fn new(session_id: String, sample_rate: u32) -> Self {
        Self {
            session_id,
            mixer: None,
            mode: MediaMode::Bridge,
            sample_rate,
        }
    }

    pub fn mode(&self) -> MediaMode {
        self.mode
    }

    pub fn mixer(&self) -> Option<&Arc<ConferenceAudioMixer>> {
        self.mixer.as_ref()
    }

    /// Switch from bridge mode to MCU mode.
    ///
    /// Creates a ConferenceAudioMixer and starts its mixing loop.
    /// The caller is responsible for:
    /// - Stopping the existing BridgePeer
    /// - Adding participants via `mixer.add_participant()`
    /// - Wiring forward/reverse loops between RTP tracks and mixer channels
    pub async fn switch_to_mcu(&mut self) -> Result<Arc<ConferenceAudioMixer>> {
        if self.mode == MediaMode::Mcu {
            if let Some(ref mixer) = self.mixer {
                return Ok(mixer.clone());
            }
        }

        info!(
            session_id = %self.session_id,
            sample_rate = self.sample_rate,
            "Switching from bridge to MCU mode"
        );

        let conf_id = format!("mcu-{}", self.session_id);
        let mixer = Arc::new(ConferenceAudioMixer::new(conf_id, self.sample_rate));
        mixer.start();

        self.mixer = Some(mixer.clone());
        self.mode = MediaMode::Mcu;

        info!(session_id = %self.session_id, "MCU mode activated");
        Ok(mixer)
    }

    /// Switch from MCU mode back to bridge mode.
    ///
    /// The caller is responsible for:
    /// - Creating a new BridgePeer
    /// - Stopping the ConferenceMediaBridge forward/reverse loops
    ///
    /// This method stops and drops the ConferenceAudioMixer.
    pub async fn switch_to_bridge(&mut self) -> Result<()> {
        if self.mode == MediaMode::Bridge {
            return Ok(());
        }

        info!(session_id = %self.session_id, "Switching from MCU to bridge mode");

        if let Some(mixer) = self.mixer.take() {
            mixer.stop().await;
        }

        self.mode = MediaMode::Bridge;

        info!(session_id = %self.session_id, "Bridge mode restored");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mcu_switch_lifecycle() {
        let mut switcher = McuSwitch::new("test-session".into(), 8000);
        assert_eq!(switcher.mode(), MediaMode::Bridge);
        assert!(switcher.mixer().is_none());

        let _mixer = switcher.switch_to_mcu().await.unwrap();
        assert_eq!(switcher.mode(), MediaMode::Mcu);
        assert!(switcher.mixer().is_some());

        // Idempotent
        let _mixer2 = switcher.switch_to_mcu().await.unwrap();
        assert_eq!(switcher.mode(), MediaMode::Mcu);

        switcher.switch_to_bridge().await.unwrap();
        assert_eq!(switcher.mode(), MediaMode::Bridge);
        assert!(switcher.mixer().is_none());
    }

    #[tokio::test]
    async fn test_mcu_switch_idempotent_bridge() {
        let mut switcher = McuSwitch::new("test".into(), 8000);
        switcher.switch_to_bridge().await.unwrap();
        assert_eq!(switcher.mode(), MediaMode::Bridge);
    }

    #[test]
    fn test_media_mode_equality() {
        assert_eq!(MediaMode::Bridge, MediaMode::Bridge);
        assert_ne!(MediaMode::Bridge, MediaMode::Mcu);
    }
}
