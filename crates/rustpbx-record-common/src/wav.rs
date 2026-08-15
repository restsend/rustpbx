//! Canonical 44-byte WAV header generation.
//!
//! Single source of truth for both recording writers (real-time
//! `CodecWavWriter` and export `write_wav_header`). Codec-specific fields
//! resolve in favor of what `rustpbx-media`'s `WavReader` accepts
//! (`format_issues`): G.722 declares 8 bits/sample (readers accept 0 or 8,
//! players prefer 8); G.729 declares 8 bits/sample with 10-byte 10 ms frames
//! (block_align = 10 × channels) — the legacy export path wrote G.729 under
//! 16-bit PCM fields, which the reader's own consistency check rejects.

use audio_codec::CodecType;

pub const FORMAT_PCM: u16 = 0x0001;
pub const FORMAT_PCMA: u16 = 0x0006;
pub const FORMAT_PCMU: u16 = 0x0007;
pub const FORMAT_G722: u16 = 0x0065;
pub const FORMAT_G729: u16 = 0x0083;

/// Output file description for [`wav_header`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavSpec {
    /// Encoded codec; `None` = 16-bit PCM.
    pub codec: Option<CodecType>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Build the 44-byte RIFF/WAVE header for `data_size` bytes of payload.
pub fn wav_header(spec: &WavSpec, data_size: u32) -> [u8; 44] {
    let format_tag: u16 = match spec.codec {
        Some(CodecType::PCMU) => FORMAT_PCMU,
        Some(CodecType::PCMA) => FORMAT_PCMA,
        Some(CodecType::G722) => FORMAT_G722,
        Some(CodecType::G729) => FORMAT_G729,
        _ => FORMAT_PCM,
    };

    // (bits_per_sample, byte_rate, block_align)
    let (bits, byte_rate, block_align): (u16, u32, u16) = match spec.codec {
        Some(CodecType::PCMU) | Some(CodecType::PCMA) => {
            let bits = 8;
            (bits, spec.sample_rate * spec.channels as u32, spec.channels)
        }
        // G.722: 64 kbps sub-ADPCM carried as 8 kHz / 8-bit.
        Some(CodecType::G722) => (8, 8000 * spec.channels as u32, spec.channels),
        // G.729: 10-byte frames per 10 ms → 1000 B/s per channel.
        Some(CodecType::G729) => (8, 1000 * spec.channels as u32, 10 * spec.channels),
        _ => {
            let bits = 16u16;
            let br = spec.sample_rate * spec.channels as u32 * (u32::from(bits) / 8);
            let ba = spec.channels * (bits / 8);
            (bits, br, ba)
        }
    };

    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36 + data_size).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    header[20..22].copy_from_slice(&format_tag.to_le_bytes());
    header[22..24].copy_from_slice(&spec.channels.to_le_bytes());
    header[24..28].copy_from_slice(&spec.sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bits.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(h: &[u8; 44]) -> (u16, u16, u32, u32, u16, u16) {
        let tag = u16::from_le_bytes([h[20], h[21]]);
        let ch = u16::from_le_bytes([h[22], h[23]]);
        let rate = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);
        let byte_rate = u32::from_le_bytes([h[28], h[29], h[30], h[31]]);
        let align = u16::from_le_bytes([h[32], h[33]]);
        let bits = u16::from_le_bytes([h[34], h[35]]);
        (tag, ch, rate, byte_rate, align, bits)
    }

    #[test]
    fn pcm_header_fields() {
        let h = wav_header(
            &WavSpec {
                codec: None,
                sample_rate: 16000,
                channels: 2,
            },
            1000,
        );
        let (tag, ch, rate, br, align, bits) = fields(&h);
        assert_eq!(
            (tag, ch, rate, br, align, bits),
            (FORMAT_PCM, 2, 16000, 64000, 4, 16)
        );
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 1000);
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 36 + 1000);
    }

    #[test]
    fn pcmu_header_fields() {
        let h = wav_header(
            &WavSpec {
                codec: Some(CodecType::PCMU),
                sample_rate: 8000,
                channels: 1,
            },
            8000,
        );
        let (tag, ch, rate, br, align, bits) = fields(&h);
        assert_eq!(
            (tag, ch, rate, br, align, bits),
            (FORMAT_PCMU, 1, 8000, 8000, 1, 8)
        );
    }

    #[test]
    fn g722_header_matches_reader_expectations() {
        let h = wav_header(
            &WavSpec {
                codec: Some(CodecType::G722),
                sample_rate: 8000,
                channels: 1,
            },
            0,
        );
        let (tag, _, _, br, _, bits) = fields(&h);
        assert_eq!(tag, FORMAT_G722);
        assert_eq!(bits, 8, "reader accepts 0 or 8; players prefer 8");
        assert_eq!(br, 8000, "64 kbps = 8000 B/s per channel");
    }

    #[test]
    fn g729_header_matches_reader_expectations() {
        let h = wav_header(
            &WavSpec {
                codec: Some(CodecType::G729),
                sample_rate: 8000,
                channels: 1,
            },
            0,
        );
        let (tag, _, _, br, align, bits) = fields(&h);
        assert_eq!(tag, FORMAT_G729);
        assert_eq!(bits, 8, "reader requires 8 bits for G.729");
        assert_eq!(align, 10, "one 10-byte G.729 frame per block");
        assert_eq!(br, 1000);
    }
}
