use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// Supervisor mode for the mixer
#[derive(Clone, Debug, PartialEq)]
pub enum SupervisorMixerMode {
    Listen,
    Whisper,
    Barge,
}

/// MediaMixer — holds supervisor session routing metadata.
///
/// Actual PCM mixing is performed by `ConferenceAudioMixer` in the
/// media engine. This type exists to track supervisor sessions and
/// their routing matrices in `MixerRegistry`.
pub struct MediaMixer {
    id: String,
    mode: Arc<Mutex<SupervisorMixerMode>>,
    started: AtomicBool,
    _sample_rate: u32,
    _channels: u16,
    _mixer: Arc<crate::media::AudioMixer>,
    cancel_token: CancellationToken,
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl MediaMixer {
    pub fn new(id: String, sample_rate: u32) -> Self {
        Self {
            id,
            mode: Arc::new(Mutex::new(SupervisorMixerMode::Listen)),
            started: AtomicBool::new(false),
            _sample_rate: sample_rate,
            _channels: 1,
            _mixer: Arc::new(crate::media::AudioMixer::new(sample_rate, 1)),
            cancel_token: CancellationToken::new(),
            task_handle: Mutex::new(None),
        }
    }

    pub fn set_mode(&self, mode: SupervisorMixerMode) {
        let mut current = self.mode.lock();
        *current = mode;
    }

    pub fn get_mode(&self) -> SupervisorMixerMode {
        self.mode.lock().clone()
    }

    pub fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }

        info!("MediaMixer {} started, mode: {:?}", self.id, self.get_mode());

        let cancel_token = self.cancel_token.clone();
        let mixer_id = self.id.clone();

        let handle = crate::utils::media_spawn(async move {
            Self::mixing_loop(&mixer_id, cancel_token).await;
        });
        *self.task_handle.lock() = Some(handle);
    }

    async fn mixing_loop(mixer_id: &str, cancel_token: CancellationToken) {
        info!("Mixing loop started for {}", mixer_id);
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!("Mixing loop cancelled for {}", mixer_id);
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    debug!("Mixing cycle for {}", mixer_id);
                }
            }
        }
        info!("Mixing loop stopped for {}", mixer_id);
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
        self.started.store(false, Ordering::SeqCst);
        info!("MediaMixer {} stopped", self.id);
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn sample_rate(&self) -> u32 {
        self._sample_rate
    }

    pub fn channels(&self) -> u16 {
        self._channels
    }

    pub fn audio_mixer(&self) -> Arc<crate::media::AudioMixer> {
        self._mixer.clone()
    }
}

impl Drop for MediaMixer {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        if let Some(handle) = self.task_handle.lock().take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mixer_creation() {
        let _mixer = MediaMixer::new("test-mixer".to_string(), 8000);
    }

    #[test]
    fn test_supervisor_mode_listen() {
        let mixer = MediaMixer::new("test".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Listen);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Listen);
    }

    #[test]
    fn test_supervisor_mode_whisper() {
        let mixer = MediaMixer::new("test".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Whisper);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Whisper);
    }

    #[test]
    fn test_supervisor_mode_barge() {
        let mixer = MediaMixer::new("test".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Barge);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Barge);
    }

    #[tokio::test]
    async fn test_mixer_start_stop() {
        let mixer = MediaMixer::new("test-start-stop".to_string(), 8000);
        mixer.start();
        mixer.stop();
        mixer.start();
        mixer.stop();
    }

    #[test]
    fn test_mixer_id_and_properties() {
        let mixer = MediaMixer::new("test-properties".to_string(), 16000);
        assert_eq!(mixer.id(), "test-properties");
        assert_eq!(mixer.sample_rate(), 16000);
        assert_eq!(mixer.channels(), 1);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Listen);
    }
}
