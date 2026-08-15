//! Coded-domain audio primitives shared by both recording pipelines.

use audio_codec::{CodecType, create_encoder};

/// How to combine two PCM legs into one mono output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixMode {
    /// Sum with clamp — used when overlaying a synthesized DTMF tone onto
    /// audio (the tone must remain audible over the speech floor).
    ClampSum,
    /// Average — used for mono mixing of two talk legs (prevents clipping
    /// when both legs are loud).
    Average,
}

/// Mix two aligned PCM (16-bit LE) buffers.
pub fn mix_pcm(a: &[u8], b: &[u8], mode: MixMode) -> Vec<u8> {
    let n = a.len().min(b.len()) / 2 * 2;
    let mut out = Vec::with_capacity(n);
    for i in (0..n).step_by(2) {
        let sa = i16::from_le_bytes([a[i], a[i + 1]]);
        let sb = i16::from_le_bytes([b[i], b[i + 1]]);
        let mixed: i16 = match mode {
            MixMode::ClampSum => {
                (i32::from(sa) + i32::from(sb)).clamp(i16::MIN as i32, i16::MAX as i32) as i16
            }
            MixMode::Average => ((i32::from(sa) + i32::from(sb)) / 2) as i16,
        };
        out.extend_from_slice(&mixed.to_le_bytes());
    }
    out
}

/// Interleave two aligned block streams (stereo output): blocks from `a`
/// become the left channel, blocks from `b` the right.
pub fn interleave_blocks(a: &[u8], b: &[u8], bytes_per_block: usize) -> Vec<u8> {
    let blocks = a.len().min(b.len()) / bytes_per_block;
    let mut out = Vec::with_capacity(blocks * bytes_per_block * 2);
    for i in 0..blocks {
        let lo = i * bytes_per_block;
        let hi = lo + bytes_per_block;
        out.extend_from_slice(&a[lo..hi]);
        out.extend_from_slice(&b[lo..hi]);
    }
    out
}

/// Encode `samples`-worth of silence in the target domain: PCM bytes for
/// PCM targets, or codec-encoded silence (a zero-PCM frame) for coded
/// targets.
pub fn silence_chunk(codec: Option<CodecType>, samples: u32) -> Vec<u8> {
    let pcm = vec![0i16; samples as usize];
    match codec {
        Some(c @ (CodecType::PCMU | CodecType::PCMA | CodecType::G729)) => {
            create_encoder(c).encode(&pcm)
        }
        _ => audio_codec::samples_to_bytes(&pcm),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_average_and_clamp() {
        let a: Vec<u8> = [1000i16, 2000i16]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let b: Vec<u8> = [3000i16, 32000i16]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let avg = mix_pcm(&a, &b, MixMode::Average);
        assert_eq!(i16::from_le_bytes([avg[0], avg[1]]), 2000);
        assert_eq!(i16::from_le_bytes([avg[2], avg[3]]), 17000);

        let sum = mix_pcm(&a, &b, MixMode::ClampSum);
        assert_eq!(i16::from_le_bytes([sum[0], sum[1]]), 4000);
        assert_eq!(i16::from_le_bytes([sum[2], sum[3]]), i16::MAX, "clamped");
    }

    #[test]
    fn interleave_orders_left_right() {
        let a = [1u8, 1, 2, 2];
        let b = [9u8, 9, 8, 8];
        let out = interleave_blocks(&a, &b, 2);
        assert_eq!(out, vec![1, 1, 9, 9, 2, 2, 8, 8]);
    }

    #[test]
    fn silence_is_zero_pcm() {
        let s = silence_chunk(None, 160);
        assert_eq!(s.len(), 320);
        assert!(s.iter().all(|&b| b == 0));
    }
}
