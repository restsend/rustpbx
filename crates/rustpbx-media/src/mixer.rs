/// Simple PCM frame mixer — pure audio mixing, no signaling dependencies.
pub struct AudioMixer;

/// Soft-knee limiter. Linear below `T` (~ -1.4 dBFS), then smoothly
/// compresses toward full scale without ever exceeding it. Replaces the
/// previous hard `clamp`, whose abrupt flattening produced harsh high-order
/// harmonics ("crackling") whenever two or more conference legs summed hot.
fn soft_clip(x: f32) -> f32 {
    // Linear-region ceiling as a fraction of full scale.
    const T: f32 = 0.85;
    let a = x.abs();
    if a <= T {
        x
    } else {
        x.signum() * (T + (1.0 - T) * ((a - T) / (1.0 - T)).tanh())
    }
}

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
            // Accumulate in f32 and soft-clip in the normalized domain: the
            // limiter's knee lives near full scale, so the sum must be scaled
            // down before limiting and back up afterwards.
            for (i, sample) in frame.iter().enumerate() {
                let mixed = soft_clip((output[i] as f32 + *sample as f32 * gain) / 32768.0);
                output[i] = (mixed * 32767.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
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

    /// Over-limit sums must stay below full scale: the soft limiter never
    /// flat-tops, so downstream encoders see no hard-clipping harmonics.
    #[test]
    fn test_audio_mixer_soft_limits_without_flat_topping() {
        let frame1 = vec![30000i16; 160];
        let frame2 = vec![30000i16; 160];
        let gains = [1.0, 1.0];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
        // Loud (well above the 30000 inputs) but never saturated.
        assert!(
            result.iter().all(|&s| s > 31000 && s < i16::MAX),
            "every sample must be loud yet unsaturated, got {:?}",
            &result[..8]
        );
    }

    /// Negative peaks must be limited symmetrically.
    #[test]
    fn test_audio_mixer_soft_limits_negative_peaks() {
        let frame1 = vec![-30000i16; 160];
        let frame2 = vec![-30000i16; 160];
        let gains = [1.0, 1.0];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        assert!(
            result.iter().all(|&s| s < -31000 && s > i16::MIN),
            "negative peaks must stay unsaturated, got {:?}",
            &result[..8]
        );
    }

    /// Moderate sums (inside the linear region) pass through untouched.
    #[test]
    fn test_audio_mixer_linear_region_untouched() {
        let frame1 = vec![10000i16; 160];
        let frame2 = vec![10000i16; 160];
        let gains = [1.0, 1.0];

        let result = AudioMixer::mix(vec![frame1, frame2], &gains);

        // The f32 round-trip (x/32768 → ×32767) loses at most one LSB.
        assert!(
            result.iter().all(|&s| (19998..=20000).contains(&s)),
            "sum below the limiter knee must pass through nearly exact, got {:?}",
            &result[..8]
        );
    }
}
