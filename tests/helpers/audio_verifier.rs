#![allow(dead_code)]

use rustpbx::media::wav_reader::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::Path;

pub fn generate_sine_wav(
    path: &Path,
    freq_hz: f64,
    duration_secs: f64,
    sample_rate: u32,
    amplitude: f64,
) {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec).unwrap();
    let num_samples = (sample_rate as f64 * duration_secs) as u64;
    for i in 0..num_samples {
        let t = i as f64 / sample_rate as f64;
        let sample =
            (amplitude * (2.0 * std::f64::consts::PI * freq_hz * t).sin() * 32767.0) as i16;
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
}

pub fn generate_dtmf_wav(
    path: &Path,
    digits: &str,
    tone_duration_ms: u32,
    gap_duration_ms: u32,
    sample_rate: u32,
    amplitude: f64,
) {
    let dtmf_freqs: std::collections::HashMap<char, (f64, f64)> = [
        ('1', (697.0, 1209.0)),
        ('2', (697.0, 1336.0)),
        ('3', (697.0, 1477.0)),
        ('4', (770.0, 1209.0)),
        ('5', (770.0, 1336.0)),
        ('6', (770.0, 1477.0)),
        ('7', (852.0, 1209.0)),
        ('8', (852.0, 1336.0)),
        ('9', (852.0, 1477.0)),
        ('*', (941.0, 1209.0)),
        ('0', (941.0, 1336.0)),
        ('#', (941.0, 1477.0)),
    ]
    .into_iter()
    .collect();

    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(path, spec).unwrap();

    let tone_samples = (sample_rate as u64 * tone_duration_ms as u64 / 1000) as usize;
    let gap_samples = (sample_rate as u64 * gap_duration_ms as u64 / 1000) as usize;

    for (idx, ch) in digits.chars().enumerate() {
        let &(f_low, f_high) = dtmf_freqs
            .get(&ch)
            .unwrap_or_else(|| panic!("invalid DTMF digit: {}", ch));

        for i in 0..tone_samples {
            let t = i as f64 / sample_rate as f64;
            let val = amplitude
                * ((2.0 * std::f64::consts::PI * f_low * t).sin()
                    + (2.0 * std::f64::consts::PI * f_high * t).sin())
                / 2.0
                * 32767.0;
            writer.write_sample(val as i16).unwrap();
        }

        if idx < digits.len() - 1 {
            for _ in 0..gap_samples {
                writer.write_sample(0i16).unwrap();
            }
        }
    }
    writer.finalize().unwrap();
}

pub fn read_wav_mono(path: &Path) -> (Vec<i16>, u32) {
    let mut reader = WavReader::open(path).unwrap();
    let sample_rate = reader.spec().sample_rate;
    let samples: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
    (samples, sample_rate)
}

pub fn read_wav_stereo(path: &Path) -> (Vec<i16>, Vec<i16>, u32) {
    let mut reader = WavReader::open(path).unwrap();
    let sample_rate = reader.spec().sample_rate;
    let all: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
    let left: Vec<i16> = all.iter().step_by(2).copied().collect();
    let right: Vec<i16> = all.iter().skip(1).step_by(2).copied().collect();
    (left, right, sample_rate)
}

pub fn compute_rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return f64::NEG_INFINITY;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| {
            let v = s as f64 / 32768.0;
            v * v
        })
        .sum();
    let mean = sum / samples.len() as f64;
    10.0 * mean.sqrt().log10()
}

pub fn compute_rms_linear(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| {
            let v = s as f64 / 32768.0;
            v * v
        })
        .sum();
    (sum / samples.len() as f64).sqrt()
}

pub fn goertzel(samples: &[i16], target_freq: f64, sample_rate: u32) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len() as f64;
    let k = (0.5 + n * target_freq / sample_rate as f64).floor();
    let omega = 2.0 * std::f64::consts::PI * k / n;
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    for &sample in samples {
        let s0 = sample as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s2 * s2 + s1 * s1 - coeff * s1 * s2).sqrt()
}

pub fn goertzel_magnitude_normalized(samples: &[i16], target_freq: f64, sample_rate: u32) -> f64 {
    let magnitude = goertzel(samples, target_freq, sample_rate);
    if samples.is_empty() {
        return 0.0;
    }
    magnitude / samples.len() as f64
}

pub fn find_dominant_frequency(
    samples: &[i16],
    sample_rate: u32,
    freq_min: f64,
    freq_max: f64,
    step: f64,
) -> (f64, f64) {
    let mut best_freq = 0.0;
    let mut best_mag = 0.0;
    let mut freq = freq_min;
    while freq <= freq_max {
        let mag = goertzel_magnitude_normalized(samples, freq, sample_rate);
        if mag > best_mag {
            best_mag = mag;
            best_freq = freq;
        }
        freq += step;
    }
    (best_freq, best_mag)
}

pub fn cross_correlate(signal: &[i16], reference: &[i16]) -> f64 {
    if signal.is_empty() || reference.is_empty() || signal.len() < reference.len() {
        return 0.0;
    }

    let ref_mean: f64 = reference.iter().map(|&s| s as f64).sum::<f64>() / reference.len() as f64;
    let ref_std: f64 = {
        let variance: f64 = reference
            .iter()
            .map(|&s| {
                let d = s as f64 - ref_mean;
                d * d
            })
            .sum::<f64>()
            / reference.len() as f64;
        variance.sqrt()
    };
    if ref_std < 1e-10 {
        return 0.0;
    }

    let window_len = reference.len();
    let max_offset = signal.len() - window_len;
    let step = std::cmp::max(1, max_offset / 200);

    let mut best_corr = 0.0f64;

    for offset in (0..=max_offset).step_by(step) {
        let window = &signal[offset..offset + window_len];
        let win_mean: f64 = window.iter().map(|&s| s as f64).sum::<f64>() / window_len as f64;
        let win_std: f64 = {
            let variance: f64 = window
                .iter()
                .map(|&s| {
                    let d = s as f64 - win_mean;
                    d * d
                })
                .sum::<f64>()
                / window_len as f64;
            variance.sqrt()
        };
        if win_std < 1e-10 {
            continue;
        }

        let numerator: f64 = window
            .iter()
            .zip(reference.iter())
            .map(|(&w, &r)| (w as f64 - win_mean) * (r as f64 - ref_mean))
            .sum();

        let corr = numerator / (window_len as f64 * win_std * ref_std);
        if corr > best_corr {
            best_corr = corr;
        }
    }

    best_corr
}

pub fn find_signal_start(samples: &[i16], threshold_linear: f64, window_size: usize) -> usize {
    let mut pos = 0;
    while pos + window_size <= samples.len() {
        let rms = compute_rms_linear(&samples[pos..pos + window_size]);
        if rms > threshold_linear {
            return pos;
        }
        pos += window_size;
    }
    samples.len()
}

pub fn extract_audio_region(
    samples: &[i16],
    sample_rate: u32,
    start_offset: usize,
    duration_ms: u32,
) -> &[i16] {
    let len = (sample_rate as u64 * duration_ms as u64 / 1000) as usize;
    let end = std::cmp::min(start_offset + len, samples.len());
    if start_offset >= samples.len() {
        return &[];
    }
    &samples[start_offset..end]
}

pub fn has_audio_content(samples: &[i16], threshold_db: f64) -> bool {
    compute_rms(samples) > threshold_db
}

pub fn resample_linear(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let new_len = (samples.len() as f64 * ratio) as usize;
    let mut result = Vec::with_capacity(new_len);
    for i in 0..new_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos as usize;
        let frac = src_pos - idx as f64;
        let s0 = samples[idx] as f64;
        let s1 = if idx + 1 < samples.len() {
            samples[idx + 1] as f64
        } else {
            s0
        };
        result.push((s0 + frac * (s1 - s0)) as i16);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("audio_verifier_test_{}_{}", std::process::id(), id))
    }

    #[test]
    fn test_generate_sine_and_read() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine_440.wav");

        generate_sine_wav(&path, 440.0, 0.5, 8000, 0.5);

        let (samples, sr) = read_wav_mono(&path);
        assert_eq!(sr, 8000);
        assert_eq!(samples.len(), 4000);

        let rms = compute_rms_linear(&samples);
        assert!(rms > 0.2, "RMS should be significant, got {}", rms);

        let (freq, mag) = find_dominant_frequency(&samples, 8000, 300.0, 600.0, 5.0);
        assert!(
            (freq - 440.0).abs() < 15.0,
            "dominant freq should be near 440Hz, got {}",
            freq
        );
        assert!(
            mag > 100.0,
            "magnitude at 440Hz should be significant, got {}",
            mag
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_goertzel_sensitivity() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine_440b.wav");

        generate_sine_wav(&path, 440.0, 1.0, 8000, 0.5);
        let (samples, sr) = read_wav_mono(&path);

        let m440 = goertzel_magnitude_normalized(&samples, 440.0, sr);
        let m1000 = goertzel_magnitude_normalized(&samples, 1000.0, sr);

        assert!(
            m440 > m1000 * 10.0,
            "440Hz magnitude should dominate 1000Hz: m440={}, m1000={}",
            m440,
            m1000
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_silence_detection() {
        let silence: Vec<i16> = vec![0; 1600];
        assert!(!has_audio_content(&silence, -40.0));

        let signal: Vec<i16> = (0..1600)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32768.0) as i16
            })
            .collect();
        assert!(has_audio_content(&signal, -40.0));
    }

    #[test]
    fn test_cross_correlate_self() {
        let signal: Vec<i16> = (0..800)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32768.0) as i16
            })
            .collect();

        let corr = cross_correlate(&signal, &signal);
        assert!(corr > 0.99, "self-correlation should be ~1.0, got {}", corr);
    }

    #[test]
    fn test_resample() {
        let samples: Vec<i16> = (0..800)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32768.0) as i16
            })
            .collect();

        let resampled = resample_linear(&samples, 8000, 16000);
        assert_eq!(resampled.len(), 1600);

        let (freq, _) = find_dominant_frequency(&resampled, 16000, 300.0, 600.0, 5.0);
        assert!(
            (freq - 440.0).abs() < 15.0,
            "resampled signal should still peak near 440Hz, got {}",
            freq
        );
    }

    #[test]
    fn test_dtmf_generation() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dtmf.wav");

        generate_dtmf_wav(&path, "12", 100, 50, 8000, 0.5);
        let (samples, sr) = read_wav_mono(&path);

        let tone_len = (8000u64 * 100 / 1000) as usize;
        let gap_len = (8000u64 * 50 / 1000) as usize;

        let digit1 = &samples[0..tone_len];
        let (freq1, _) = find_dominant_frequency(digit1, sr, 600.0, 1500.0, 5.0);

        let digit2_start = tone_len + gap_len;
        let digit2 = &samples[digit2_start..digit2_start + tone_len];
        let (freq2, _) = find_dominant_frequency(digit2, sr, 600.0, 1500.0, 5.0);

        assert!(
            (freq1 - 697.0).abs() < 20.0 || (freq1 - 1209.0).abs() < 20.0,
            "digit 1 should have 697Hz or 1209Hz component, dominant={}",
            freq1
        );
        let _ = freq2;

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_signal_start() {
        let mut samples: Vec<i16> = vec![0; 3200];
        for i in 0..800 {
            let t = i as f64 / 8000.0;
            samples[2400 + i] =
                (0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32768.0) as i16;
        }

        let start = find_signal_start(&samples, 0.01, 160);
        assert!(
            start <= 2560 && start >= 2240,
            "signal start should be around 2400, got {}",
            start
        );
    }
}
