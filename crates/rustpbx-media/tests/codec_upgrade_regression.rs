//! Regression tests pinning audio-codec 0.4.9 behaviour (upgraded from 0.4.7).
//!
//! Two upstream changes matter to rustpbx:
//! 1. Resampler integer-tick timing (0.4.9, upstream fix #4): 20 ms chunks
//!    resampled up to 48 kHz must yield EXACTLY 960 samples. The pre-0.4.9
//!    f32/Q24 phase accumulator drifted on the 1/6 (8k→48k) and 1/3 (16k→48k)
//!    ratios and periodically produced 959/961-sample chunks, which the Opus
//!    encoder rejects — `encode` returned an empty payload mid-call.
//! 2. g729-sys 0.2.1: the G.729 decoder must survive truncated / garbage
//!    bit-streams without panicking.

use audio_codec::{create_decoder, create_encoder, BoxedResampler, CodecType, Decoder, Encoder};

/// Deterministic 440 Hz sine at `rate`, phase-continuous across chunks via
/// `start_index`.
fn sine_chunk(rate: u32, start_index: usize, len: usize) -> Vec<i16> {
    let amp = i16::MAX as f32 * 0.3;
    (0..len)
        .map(|i| {
            let t = (start_index + i) as f32 / rate as f32;
            (amp * (core::f32::consts::TAU * 440.0 * t).sin()) as i16
        })
        .collect()
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// 400 s of 8 kHz audio streamed through a long-lived resampler in 20 ms
/// chunks: every chunk must produce exactly 960 samples at 48 kHz. The old
/// implementation drifted late (959 samples) roughly every ~175 s.
#[test]
fn resampler_8k_to_48k_exact_20ms_counts_over_long_stream() {
    let mut resampler = BoxedResampler::new(8000, 48_000).expect("valid rates");
    let chunks = 20_000; // 400 s — well past the old ~175 s drift period
    let mut total_out = 0usize;
    for c in 0..chunks {
        let input = sine_chunk(8000, c * 160, 160);
        let out = resampler.resample(&input);
        assert_eq!(out.len(), 960, "8k→48k chunk {c} produced {} samples", out.len());
        total_out += out.len();
    }
    assert_eq!(total_out, chunks * 960);
}

/// 720 s of 16 kHz audio: every 20 ms chunk (320 samples in) must produce
/// exactly 960 samples. The old implementation drifted early (961 samples)
/// roughly every ~350 s.
#[test]
fn resampler_16k_to_48k_exact_20ms_counts_over_long_stream() {
    let mut resampler = BoxedResampler::new(16_000, 48_000).expect("valid rates");
    let chunks = 36_000; // 720 s — past the old ~350 s drift period
    let mut total_out = 0usize;
    for c in 0..chunks {
        let input = sine_chunk(16_000, c * 320, 320);
        let out = resampler.resample(&input);
        assert_eq!(out.len(), 960, "16k→48k chunk {c} produced {} samples", out.len());
        total_out += out.len();
    }
    assert_eq!(total_out, chunks * 960);
}

/// The production failure mode fixed by 0.4.9: a prompt decoder at 8 kHz
/// feeding an Opus leg at 48 kHz. Every resampled 20 ms frame must encode to
/// a non-empty payload (no dropped frames), and the payload must decode back
/// to a full 960-sample frame. Covers both the mono encoder used by egress
/// (`OpusEncoder::new(48_000, 1)`) and the default stereo encoder returned by
/// `create_encoder(CodecType::Opus)` (used by the record/media paths).
#[test]
fn opus_encode_of_resampled_20ms_frames_never_yields_empty_payload() {
    let mut resampler = BoxedResampler::new(8000, 48_000).expect("valid rates");
    let mut mono_encoder = audio_codec::opus::OpusEncoder::new(48_000, 1);
    let mut default_encoder = create_encoder(CodecType::Opus);
    let mut decoder = create_decoder(CodecType::Opus);

    let chunks = 2_000; // 40 s of audio
    for c in 0..chunks {
        let input = sine_chunk(8000, c * 160, 160);
        let frame = resampler.resample(&input);
        assert_eq!(frame.len(), 960, "chunk {c}: bad resampled frame size");

        let mono_payload = mono_encoder.encode(&frame);
        assert!(!mono_payload.is_empty(), "chunk {c}: mono Opus dropped the frame");

        let default_payload = default_encoder.encode(&frame);
        assert!(!default_payload.is_empty(), "chunk {c}: default Opus dropped the frame");

        if c == 0 {
            let pcm = decoder.decode(&mono_payload);
            assert_eq!(pcm.len(), 960, "decoded 20 ms Opus frame must be 960 samples");
            assert!(rms(&pcm) > 100.0, "decoded frame must not be silent");
        }
    }
}

/// G.729 round-trip through g729-sys 0.2.1: 80-sample/10 ms frames encode to
/// 10-byte payloads and decode back to 80 samples with the signal preserved.
#[test]
fn g729_round_trip_preserves_payload_framing_and_signal() {
    let mut encoder = create_encoder(CodecType::G729);
    let mut decoder = create_decoder(CodecType::G729);

    let frames = 200; // 2 s of 10 ms frames
    let mut payload = Vec::with_capacity(frames * 10);
    for f in 0..frames {
        let frame = sine_chunk(8000, f * 80, 80);
        let encoded = encoder.encode(&frame);
        assert_eq!(encoded.len(), 10, "G.729 frame {f}: payload must be 10 bytes");
        payload.extend_from_slice(&encoded);
    }

    let decoded = decoder.decode(&payload);
    assert_eq!(decoded.len(), frames * 80, "must decode back to 16000 samples");
    let in_rms = rms(&sine_chunk(8000, 0, frames * 80));
    let out_rms = rms(&decoded);
    assert!(out_rms > in_rms * 0.05, "G.729 output collapsed: rms {out_rms} vs input {in_rms}");
    assert!(out_rms < in_rms * 1.5, "G.729 output blew up: rms {out_rms} vs input {in_rms}");
}

/// g729-sys 0.2.1 hardening: truncated and garbage bit-streams must not panic
/// and must not yield bogus sample counts.
#[test]
fn g729_decoder_survives_truncated_and_garbage_payloads() {
    let mut decoder = create_decoder(CodecType::G729);

    // Empty input → empty output.
    let pcm = decoder.decode(&[]);
    assert!(pcm.is_empty());

    // Truncated frame (5 bytes < 10-byte frame) → no panic, no partial frame.
    let pcm = decoder.decode(&[0xAB, 0xCD, 0xEF, 0x01, 0x02]);
    assert!(pcm.is_empty(), "truncated frame must not produce samples");

    // Full-length garbage frame → no panic, exactly one frame of samples.
    let pcm = decoder.decode(&[0xAB; 10]);
    assert_eq!(pcm.len(), 80, "garbage frame must yield exactly one 80-sample frame");

    // Trailing garbage after valid frames is dropped, not decoded partially.
    let mut encoder = create_encoder(CodecType::G729);
    let mut payload = Vec::new();
    for f in 0..3 {
        payload.extend_from_slice(&encoder.encode(&sine_chunk(8000, f * 80, 80)));
    }
    payload.extend_from_slice(&[0x00; 7]); // 7 trailing bytes: not a full frame
    let pcm = decoder.decode(&payload);
    assert_eq!(pcm.len(), 240, "3 valid frames + 7 truncated bytes → 240 samples");
}
