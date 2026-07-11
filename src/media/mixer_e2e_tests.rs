// E2E tests for the audio mixer module
// These tests verify the complete audio mixing pipeline

#[cfg(test)]
mod mixer_e2e_tests {
    use crate::media::mixer::AudioMixer;
    use crate::proxy::proxy_call::mixer::{MediaMixer, SupervisorMixerMode};

    #[test]
    fn test_mixing_with_different_gains() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![1000i16; 160];
        let frame2 = vec![1000i16; 160];
        let frame3 = vec![1000i16; 160];

        let gains = [1.0, 0.5, 0.0];

        let result = mixer.mix_frames(vec![frame1, frame2, frame3], &gains);

        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| (1400..=1600).contains(&s)));
    }

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

    #[test]
    fn test_mixer_saturation_at_boundary() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![i16::MAX; 160];
        let frame2 = vec![i16::MAX; 160];
        let frame3 = vec![i16::MAX; 160];

        let result = mixer.mix_frames(vec![frame1, frame2, frame3], &[1.0, 1.0, 1.0]);

        assert!(result.iter().all(|&s| s == i16::MAX));
    }

    #[test]
    fn test_mixer_negative_samples() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![-1000i16; 160];
        let frame2 = vec![-500i16; 160];

        let result = mixer.mix_frames(vec![frame1, frame2], &[1.0, 1.0]);

        assert!(result.iter().all(|&s| s < -1400 && s > -1600));
    }

    #[test]
    fn test_gain_application_in_mixer() {
        let mixer = AudioMixer::new(8000, 1);

        let frame = vec![1000i16; 160];

        let result = mixer.mix_frames(vec![frame.clone()], &[2.0]);
        assert!(result.iter().all(|&s| (1900..=2100).contains(&s)));

        let result = mixer.mix_frames(vec![frame], &[0.5]);
        assert!(result.iter().all(|&s| (450..=550).contains(&s)));
    }

    #[tokio::test]
    async fn test_mixer_start_stop_lifecycle() {
        let mixer = MediaMixer::new("test-lifecycle".to_string(), 8000);

        mixer.start();
        mixer.stop();

        mixer.start();
        mixer.stop();
    }
}
