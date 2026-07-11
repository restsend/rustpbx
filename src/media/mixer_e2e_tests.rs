// E2E tests for the audio mixer module
// These tests verify the complete audio mixing pipeline

#[cfg(test)]
mod mixer_e2e_tests {
    use crate::media::mixer::AudioMixer;

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

}
