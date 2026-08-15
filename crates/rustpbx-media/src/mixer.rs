/// Simple PCM frame mixer — pure audio mixing, no signaling dependencies.
pub struct AudioMixer;

impl AudioMixer {
    pub fn mix_frames(&self, frames: Vec<Vec<i16>>, gains: &[f32]) -> Vec<i16> {
        Self::mix(frames, gains)
    }

    pub fn mix(frames: Vec<Vec<i16>>, gains: &[f32]) -> Vec<i16> {
        if frames.is_empty() || gains.len() != frames.len() {
            return vec![];
        }

        let frame_len = frames[0].len();
        let mut output = vec![0i16; frame_len];

        for (frame, &gain) in frames.iter().zip(gains) {
            if frame.len() != frame_len {
                continue;
            }
            for (i, sample) in frame.iter().enumerate() {
                let mixed = (output[i] as f32 + (*sample as f32) * gain) as i16;
                output[i] = mixed.clamp(i16::MIN, i16::MAX);
            }
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_mixer_basic() {
        let frame1 = vec![1000i16; 160];
        let frame2 = vec![500i16; 160];
        let gains = [1.0, 1.0];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| s > 1000));
    }

    #[test]
    fn test_audio_mixer_with_gain() {
        let frame1 = vec![1000i16; 160];
        let frame2 = vec![1000i16; 160];
        let gains = [1.0, 0.5];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
    }

    #[test]
    fn test_audio_mixer_empty() {
        let result = AudioMixer::mix(vec![], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_audio_mixer_mix_frames_with_zero_gain() {
        let frame1 = vec![1000i16; 160];
        let frame2 = vec![1000i16; 160];
        let gains = [1.0, 0.0];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| (900..=1100).contains(&s)));
    }

    #[test]
    fn test_audio_mixer_mix_multiple_frames() {
        let frame1 = vec![100i16; 160];
        let frame2 = vec![100i16; 160];
        let frame3 = vec![100i16; 160];
        let gains = [1.0, 1.0, 1.0];

        let result = AudioMixer::mix(vec![frame1, frame2, frame3], &gains);

        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| (250..=350).contains(&s)));
    }

    #[test]
    fn test_audio_mixer_saturation_handling() {
        let frame1 = vec![30000i16; 160];
        let frame2 = vec![30000i16; 160];
        let gains = [1.0, 1.0];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| s == i16::MAX));
    }
}
