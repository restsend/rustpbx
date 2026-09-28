//! Audio content analysis for receive-path RTP verification.
//!
//! Mirrors the "anti-fake-e2e" philosophy of the Python regression helpers
//! (`regression/helpers/audio_verifier.py`): assert on the *content* of the
//! received media (dominant frequency / loudness), not just packet counts, so
//! a silent stream can never fake a pass. The Rust-native implementation
//! works on μ-law (PCMU) RTP payloads captured straight off the wire.

/// Decode a single μ-law (G.711 PCMU) byte into a linear 16-bit PCM sample.
///
/// Classic G.711 expansion: invert, then sign + 3-bit exponent + 4-bit
/// mantissa with the 0x84 bias add-back (see ITU-T G.711 / Sun `g711.c`).
pub fn mulaw_to_linear(byte: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !byte;
    let mut magnitude = (((u & 0x0F) as i32) << 3) + BIAS;
    magnitude <<= ((u & 0x70) >> 4) as i32;
    let value = if u & 0x80 != 0 {
        BIAS - magnitude
    } else {
        magnitude - BIAS
    };
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Decode one RTP payload worth of μ-law bytes into linear samples.
pub fn mulaw_payload_to_samples(payload: &[u8]) -> Vec<i16> {
    payload.iter().map(|&b| mulaw_to_linear(b)).collect()
}

/// Root mean square of linear samples (0.0 for empty input).
pub fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

/// Goertzel power of `freq` over `samples` (power normalized per sample so
/// same-rate windows of different lengths stay comparable).
pub fn goertzel_power(samples: &[i16], sample_rate: u32, freq: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let omega = 2.0 * std::f64::consts::PI * freq / sample_rate as f64;
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0f64;
    let mut s2 = 0.0f64;
    for &sample in samples {
        let s0 = sample as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power / samples.len() as f64
}

/// Pick the strongest candidate frequency in `samples`.
/// Returns `(freq, power)`; `(0.0, 0.0)` when `candidates` is empty.
pub fn dominant_frequency(samples: &[i16], sample_rate: u32, candidates: &[f64]) -> (f64, f64) {
    candidates
        .iter()
        .copied()
        .map(|f| {
            let power = goertzel_power(samples, sample_rate, f);
            (f, power)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or((0.0, 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, sample_rate: u32, samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let t = i as f64 / sample_rate as f64;
                (8000.0 * (2.0 * std::f64::consts::PI * freq * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn mulaw_silence_decodes_to_zero() {
        // 0xFF and 0x7F are the two μ-law zero codes.
        assert_eq!(mulaw_to_linear(0xFF), 0);
        assert_eq!(mulaw_to_linear(0x7F), 0);
    }

    #[test]
    fn dominant_frequency_picks_the_tone() {
        let samples = sine(600.0, 8000, 8000);
        let (freq, _) = dominant_frequency(&samples, 8000, &[300.0, 440.0, 600.0, 900.0]);
        assert!((freq - 600.0).abs() < 50.0, "got {freq}");
    }

    #[test]
    fn dominant_frequency_discriminates_neighbors() {
        let samples = sine(440.0, 8000, 8000);
        let (freq, _) = dominant_frequency(&samples, 8000, &[440.0, 480.0, 600.0]);
        assert!((freq - 440.0).abs() < 50.0, "got {freq}");
    }

    #[test]
    fn rms_of_tone_exceeds_silence() {
        let tone = sine(600.0, 8000, 8000);
        assert!(rms(&tone) > 1000.0);
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&vec![0i16; 8000]), 0.0);
    }
}
