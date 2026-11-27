//! Discontinuous Transmission (DTX) and Comfort Noise (CN) classification and
//! decoding helpers shared across `rustpbx-media` decode pipelines.
//!
//! Handles:
//! - RFC 3389 narrowband Comfort Noise (static payload type 13)
//! - G.729 Annex B 2-byte SID frames and mixed `10*N + 2` speech+SID payloads
//!   (RFC 3551 §4.5.6)
//! - Opus short DTX markers (RFC 6716 §3.2) without triggering `opus-rs` PLC
//!   output or resetting the decoder's channel count

use audio_codec::{CodecType, Decoder};

/// RFC 3389 narrowband Comfort Noise static RTP payload type.
pub const COMFORT_NOISE_PT: u8 = 13;

/// Lightweight DTX state transition implied by one inbound RTP audio packet
/// (used during startup queue drain without decoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketDtxSignal {
    /// Packet enters or continues DTX silence (PT 13 CN, G.729B SID / mixed
    /// speech+SID, or a valid short Opus DTX marker).
    EnterDtx,
    /// Active speech packet (clears DTX).
    Speech,
    /// Non-audio / malformed packet — leave the current DTX state unchanged.
    Ignore,
}

/// DTX-aware decode result for one inbound RTP audio packet.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodedAudio {
    /// Active mono speech PCM at `decoder.sample_rate()`.
    /// When `enter_dtx` is `true`, a trailing 10 ms G.729 Annex B SID followed
    /// the speech frames (decoder state was synced and the SID's CNG samples
    /// were stripped so callers can transition cleanly into DTX silence).
    Speech {
        pcm: Vec<i16>,
        enter_dtx: bool,
    },
    /// Explicit DTX / comfort-noise packet (PT 13 CN, standalone G.729B 2-byte
    /// SID, or valid short Opus DTX marker). Decoder state was updated where
    /// applicable without emitting CNG or PLC samples.
    Silence,
    /// Empty or malformed packet — leave DTX state unchanged.
    Ignore,
}

/// Classify a short Opus packet (1–2 bytes) from its RFC 6716 §3.2 framing.
///
/// - `Some(true)`: valid empty/DTX packet (code 0/1 TOC-only, code 2 `[toc, 0]`,
///   code 3 `[toc, 0x81]` VBR or `[toc, count]` CBR with total duration <= 120 ms).
/// - `Some(false)`: valid coded 2-byte packet (code 0 with 1 coded payload byte).
/// - `None`: malformed framing; callers must leave DTX state unchanged.
pub fn short_opus_dtx(data: &[u8]) -> Option<bool> {
    let (&toc, rest) = data.split_first()?;
    match (toc & 3, rest) {
        (0, []) | (1, []) | (2, [0]) | (3, [0x81]) => Some(true),
        // A coded byte may carry audio even when the whole packet is 2 bytes.
        (0, [_]) => Some(false),
        (3, [count @ 1..=48]) => {
            // Code 3 can carry multiple empty CBR frames, up to 120 ms total.
            let config = (toc >> 3) as usize;
            let frame_duration = match config {
                0..=11 => [4, 8, 16, 24][config & 3],
                12..=15 => [4, 8][config & 1],
                _ => 1 << (config & 3),
            };
            (usize::from(*count) * frame_duration <= 48).then_some(true) // 2.5 ms units
        }
        _ => None,
    }
}

/// Classify one inbound RTP packet's DTX effect without decoding it.
///
/// Only packets matching `audio_pt` (or `None` PT in unit fixtures) and RFC 3389
/// PT 13 Comfort Noise affect DTX state; DTMF and other payload types return
/// [`PacketDtxSignal::Ignore`].
pub fn classify_rtp_audio_dtx(
    codec: CodecType,
    audio_pt: u8,
    packet_pt: Option<u8>,
    data: &[u8],
) -> PacketDtxSignal {
    if packet_pt == Some(COMFORT_NOISE_PT) && audio_pt != COMFORT_NOISE_PT {
        return PacketDtxSignal::EnterDtx;
    }
    if packet_pt.is_some_and(|pt| pt != audio_pt) {
        return PacketDtxSignal::Ignore;
    }
    classify_codec_payload_dtx(codec, data)
}

/// Classify a payload belonging to `codec` without decoding it.
pub fn classify_codec_payload_dtx(codec: CodecType, data: &[u8]) -> PacketDtxSignal {
    if data.is_empty() {
        return PacketDtxSignal::Ignore;
    }
    match codec {
        CodecType::G729 if codec.is_silence_frame(data) => PacketDtxSignal::EnterDtx,
        CodecType::G729 if data.len() < 10 => PacketDtxSignal::Ignore,
        CodecType::Opus if codec.is_silence_frame(data) => match short_opus_dtx(data) {
            Some(true) => PacketDtxSignal::EnterDtx,
            Some(false) => PacketDtxSignal::Speech,
            None => PacketDtxSignal::Ignore,
        },
        _ => PacketDtxSignal::Speech,
    }
}

/// Decode one RTP audio payload with DTX awareness while keeping stateful
/// decoders (`G729`, `Opus`) synchronized.
///
/// - RFC 3389 PT 13 CN returns [`DecodedAudio::Silence`] without touching `decoder`.
/// - Standalone G.729B 2-byte SID syncs `decoder` and returns [`DecodedAudio::Silence`].
/// - Mixed G.729B `10*N + 2` speech+SID syncs `decoder`, strips the trailing
///   80 SID CNG samples, and returns [`DecodedAudio::Speech`] with `enter_dtx: true`.
/// - Valid short Opus DTX preserves `decoder.channels()` so the TOC stereo bit
///   cannot reset the decoder, advances decoder state when the marker fits its
///   20 ms decode buffer, discards the PLC output, and returns [`DecodedAudio::Silence`].
/// - Malformed short Opus packets and truncated G.729 payloads return [`DecodedAudio::Ignore`].
pub fn decode_audio_packet(
    codec: CodecType,
    decoder: &mut dyn Decoder,
    data: &[u8],
) -> DecodedAudio {
    if data.is_empty() {
        return DecodedAudio::Ignore;
    }

    match codec {
        CodecType::Opus if codec.is_silence_frame(data) => match short_opus_dtx(data) {
            Some(true) => {
                // Keep the decoder's current channel count so an empty DTX
                // marker with a different TOC stereo bit cannot reset it.
                let stereo_bit = u8::from(decoder.channels() == 2) << 2;
                let marker = [
                    (data[0] & !0x04) | stereo_bit,
                    data.get(1).copied().unwrap_or(0),
                ];
                // Advance decoder state when possible (valid multi-frame
                // markers can exceed `OpusDecoder`'s 20 ms decode window).
                let mut scratch = [0i16; 960 * 2];
                let _ = decoder.decode_into(&marker[..data.len()], &mut scratch);
                DecodedAudio::Silence
            }
            Some(false) => {
                let pcm = decoder.decode(data);
                if pcm.is_empty() {
                    DecodedAudio::Ignore
                } else {
                    DecodedAudio::Speech {
                        pcm,
                        enter_dtx: false,
                    }
                }
            }
            None => DecodedAudio::Ignore,
        },
        CodecType::G729 => {
            let has_sid = codec.is_silence_frame(data);
            if !has_sid && data.len() < 10 {
                return DecodedAudio::Ignore;
            }
            if has_sid && data.len() == 2 {
                // Standalone 2-byte Annex B SID: sync decoder state, suppress CNG.
                let mut scratch = [0i16; 80];
                let _ = decoder.decode_into(data, &mut scratch);
                return DecodedAudio::Silence;
            }
            let mut pcm = decoder.decode(data);
            if has_sid {
                // RFC 3551 §4.5.6: trailing 2-byte SID decoded to 80 CNG samples.
                let speech_len = pcm.len().saturating_sub(80);
                pcm.truncate(speech_len);
                if pcm.is_empty() {
                    return DecodedAudio::Silence;
                }
                return DecodedAudio::Speech {
                    pcm,
                    enter_dtx: true,
                };
            }
            if pcm.is_empty() {
                DecodedAudio::Ignore
            } else {
                DecodedAudio::Speech {
                    pcm,
                    enter_dtx: false,
                }
            }
        }
        _ => {
            let pcm = decoder.decode(data);
            if pcm.is_empty() {
                DecodedAudio::Ignore
            } else {
                DecodedAudio::Speech {
                    pcm,
                    enter_dtx: false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{create_decoder, create_encoder};

    fn sine_pcm(sample_rate: u32, samples: usize, freq_hz: f32) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((t * freq_hz * std::f32::consts::TAU).sin() * 12_000.0) as i16
            })
            .collect()
    }

    #[test]
    fn short_opus_dtx_matches_rfc6716_framing() {
        assert_eq!(short_opus_dtx(&[0xF8]), Some(true)); // code 0, empty
        assert_eq!(short_opus_dtx(&[0xF9]), Some(true)); // code 1, two empty
        assert_eq!(short_opus_dtx(&[0xFA, 0]), Some(true)); // code 2, two empty
        assert_eq!(short_opus_dtx(&[0xFB, 0x81]), Some(true)); // code 3, 1 VBR empty
        assert_eq!(short_opus_dtx(&[0xFB, 2]), Some(true)); // code 3, 2x20ms CBR empty
        assert_eq!(short_opus_dtx(&[0xFB, 6]), Some(true)); // code 3, 6x20ms = 120ms
        assert_eq!(short_opus_dtx(&[0xFB, 7]), None); // 7x20ms > 120ms invalid
        assert_eq!(short_opus_dtx(&[0xFA]), None); // code 2 missing length byte
        assert_eq!(short_opus_dtx(&[0xF9, 0]), None); // code 1 odd payload byte
        assert_eq!(short_opus_dtx(&[0xFB, 0x82]), None); // code 3 VBR 2 frames missing len
        assert_eq!(short_opus_dtx(&[0xF8, 0]), Some(false)); // code 0, 1 coded byte
        assert_eq!(short_opus_dtx(&[]), None);
    }

    #[test]
    fn classify_rtp_audio_dtx_handles_cn_g729_opus_and_dtmf() {
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::PCMU, 0, Some(COMFORT_NOISE_PT), &[40]),
            PacketDtxSignal::EnterDtx
        );
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::G729, 18, Some(18), &[0x12, 0x34]),
            PacketDtxSignal::EnterDtx
        );
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::G729, 18, Some(18), &[0; 22]),
            PacketDtxSignal::EnterDtx
        );
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::G729, 18, Some(18), &[0; 5]),
            PacketDtxSignal::Ignore
        );
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::G729, 18, Some(18), &[0; 20]),
            PacketDtxSignal::Speech
        );
        // DTMF packet does not alter DTX state.
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::G729, 18, Some(101), &[1, 0x8a, 0, 160]),
            PacketDtxSignal::Ignore
        );
        // Opus DTX vs malformed vs coded 2-byte frame.
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::Opus, 111, Some(111), &[0xF8]),
            PacketDtxSignal::EnterDtx
        );
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::Opus, 111, Some(111), &[0xFA]),
            PacketDtxSignal::Ignore
        );
        assert_eq!(
            classify_rtp_audio_dtx(CodecType::Opus, 111, Some(111), &[0xF8, 0]),
            PacketDtxSignal::Speech
        );
    }

    #[test]
    fn decode_audio_packet_strips_g729_sid_and_keeps_opus_channel_state() {
        let pcm_8k = sine_pcm(8_000, 160, 440.0);
        let g729_20ms = create_encoder(CodecType::G729).encode(&pcm_8k);
        assert_eq!(g729_20ms.len(), 20);
        let mut g729_dec = create_decoder(CodecType::G729);

        // Standalone 2-byte SID → Silence.
        assert_eq!(
            decode_audio_packet(CodecType::G729, g729_dec.as_mut(), &[0x12, 0x34]),
            DecodedAudio::Silence
        );
        // Mixed 12-byte (10 ms speech + 2 B SID) → 80 speech samples + enter_dtx.
        let mut mixed_12 = g729_20ms[..10].to_vec();
        mixed_12.extend_from_slice(&[0x12, 0x34]);
        match decode_audio_packet(CodecType::G729, g729_dec.as_mut(), &mixed_12) {
            DecodedAudio::Speech { pcm, enter_dtx } => {
                assert_eq!(pcm.len(), 80);
                assert!(enter_dtx);
            }
            other => panic!("expected Speech, got {other:?}"),
        }
        // Mixed 22-byte (20 ms speech + 2 B SID) → 160 speech samples + enter_dtx.
        let mut mixed_22 = g729_20ms.clone();
        mixed_22.extend_from_slice(&[0x12, 0x34]);
        match decode_audio_packet(CodecType::G729, g729_dec.as_mut(), &mixed_22) {
            DecodedAudio::Speech { pcm, enter_dtx } => {
                assert_eq!(pcm.len(), 160);
                assert!(enter_dtx);
            }
            other => panic!("expected Speech, got {other:?}"),
        }

        // Opus: DTX marker with flipped stereo bit must not reset decoder state.
        let mut enc = create_encoder(CodecType::Opus);
        let speech = enc.encode(&sine_pcm(48_000, 960, 440.0));
        let resumed = enc.encode(&sine_pcm(48_000, 960, 660.0));
        let marker = [speech[0] & !3];
        let flipped_channel_marker = [marker[0] ^ 0x04];

        let mut reference = create_decoder(CodecType::Opus);
        reference.decode(&speech);
        reference.decode(&marker);
        let expected = reference.decode(&resumed);

        let mut opus_dec = create_decoder(CodecType::Opus);
        assert!(matches!(
            decode_audio_packet(CodecType::Opus, opus_dec.as_mut(), &speech),
            DecodedAudio::Speech { enter_dtx: false, .. }
        ));
        assert_eq!(
            decode_audio_packet(CodecType::Opus, opus_dec.as_mut(), &flipped_channel_marker),
            DecodedAudio::Silence
        );
        match decode_audio_packet(CodecType::Opus, opus_dec.as_mut(), &resumed) {
            DecodedAudio::Speech { pcm, enter_dtx } => {
                assert_eq!(pcm, expected);
                assert!(!enter_dtx);
            }
            other => panic!("expected resumed Opus Speech, got {other:?}"),
        }
    }
}
