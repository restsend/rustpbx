/// Linear region of the output transfer curve, as a fraction of full scale.
const SOFT_CLIP_KNEE: f32 = 0.85;
/// Full-scale magnitude used to normalise the f32 accumulator (one lsb of
/// headroom below `i16::MAX` keeps positive/negative saturation symmetric).
const FULL_SCALE: f32 = 32_768.0;

/// Soft limiter applied to the summed mix.
///
/// Below the knee the curve is exactly linear (bit-transparent apart from
/// rounding). Above it the excess is folded through `tanh`, so a peak that is
/// a few dB over full scale is rounded instead of being sliced flat: hard
/// clipping (`clamp`) turns simultaneous talkers into broadband harmonic
/// distortion, heard as "破音/发炸".
pub(crate) fn soft_clip_sample(sample: f32) -> i16 {
    let knee = FULL_SCALE * SOFT_CLIP_KNEE;
    let magnitude = sample.abs();
    let limited = if magnitude <= knee {
        sample
    } else {
        let over = (magnitude - knee) / (FULL_SCALE - knee);
        sample.signum() * (knee + (FULL_SCALE - knee) * over.tanh())
    };
    limited.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

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
        // Accumulate in f32 so intermediate sums cannot clip each other
        // before the limiter sees the true mix level.
        let mut output = vec![0f32; frame_len];

        for (frame, &gain) in frames.iter().zip(gains) {
            if frame.len() != frame_len {
                continue;
            }
            for (accumulator, &sample) in output.iter_mut().zip(frame.iter()) {
                *accumulator += sample as f32 * gain;
            }
        }

        output.into_iter().map(soft_clip_sample).collect()
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
        // Full-scale overload still reaches the rail, but through the soft
        // limiter (the old hard clamp is kept as the final safety bound).
        assert!(result.iter().all(|&s| s <= i16::MAX));
        assert!(result.iter().all(|&s| s > 30_000));
    }

    #[test]
    fn soft_limiter_is_linear_below_knee_and_bounded_above() {
        // Quiet audio must be bit-transparent (apart from f32 rounding).
        for sample in [-20_000.0, -5_000.0, 0.0, 5_000.0, 20_000.0] {
            let clipped = soft_clip_sample(sample);
            assert!(
                (clipped as f32 - sample).abs() <= 1.0,
                "{sample} must pass through the linear region unchanged, got {clipped}"
            );
        }

        // Overload is compressed but never wraps or exceeds full scale.
        let mut previous = i16::MIN;
        for sample in [
            -120_000.0, -60_000.0, -40_000.0, -30_000.0, -20_000.0, 0.0, 20_000.0, 30_000.0,
            40_000.0, 60_000.0, 120_000.0,
        ] {
            let clipped = soft_clip_sample(sample);
            assert!(clipped >= previous, "limiter must be monotonic");
            previous = clipped;
        }
        assert_eq!(soft_clip_sample(i16::MAX as f32 * 4.0), i16::MAX);
        assert_eq!(soft_clip_sample(i16::MIN as f32 * 4.0), i16::MIN);
    }
}
