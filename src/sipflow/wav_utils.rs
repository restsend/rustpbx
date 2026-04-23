use anyhow::{Result, anyhow};
use audio_codec::{CodecType, Decoder, Resampler, create_decoder, create_encoder};
use rustrtc::rtp::RtpPacket;
use std::{
    collections::{BTreeMap, HashMap},
    io::{Cursor, Seek, SeekFrom, Write},
};

use crate::media::{
    StreamWriter, negotiate::MediaNegotiator, recorder::DtmfGenerator, wav_writer::WavWriter,
};
use crate::sipflow::{SipFlowItem, SipFlowMsgType, extract_rtp_addr, extract_sdp};

#[derive(Debug, Clone, Copy)]
pub(crate) struct PayloadDescriptor {
    codec: CodecType,
    clock_rate: u32,
}

pub(crate) type PayloadTypeMap = HashMap<u8, PayloadDescriptor>;
pub(crate) type LegPayloadTypeMap = HashMap<i32, PayloadTypeMap>;

fn default_payload_descriptor(pt: u8) -> PayloadDescriptor {
    match pt {
        0 => PayloadDescriptor {
            codec: CodecType::PCMU,
            clock_rate: 8000,
        },
        8 => PayloadDescriptor {
            codec: CodecType::PCMA,
            clock_rate: 8000,
        },
        9 => PayloadDescriptor {
            codec: CodecType::G722,
            clock_rate: 8000,
        },
        18 => PayloadDescriptor {
            codec: CodecType::G729,
            clock_rate: 8000,
        },
        96 | 111 => PayloadDescriptor {
            codec: CodecType::Opus,
            clock_rate: 48000,
        },
        _ => PayloadDescriptor {
            codec: CodecType::PCMU,
            clock_rate: 8000,
        },
    }
}

pub(crate) fn build_payload_type_map(flow_items: &[SipFlowItem]) -> PayloadTypeMap {
    let mut map = HashMap::new();

    for item in flow_items {
        if item.msg_type != SipFlowMsgType::Sip {
            continue;
        }

        let Ok(message) = std::str::from_utf8(&item.payload) else {
            continue;
        };
        let Some(sdp) = extract_sdp(message) else {
            continue;
        };

        for info in MediaNegotiator::extract_all_codecs(&sdp) {
            map.entry(info.payload_type).or_insert(PayloadDescriptor {
                codec: info.codec,
                clock_rate: info.clock_rate,
            });
        }
    }

    map
}

fn media_port(addr: &str) -> Option<u16> {
    addr.rsplit_once(':')?.1.parse().ok()
}

fn infer_leg_for_sdp(sdp: &str, leg_sources: &HashMap<i32, Vec<String>>) -> Option<i32> {
    let rtp_addr = extract_rtp_addr(sdp)?;
    let rtp_port = media_port(&rtp_addr)?;
    let mut matched_leg = None;

    for (leg, sources) in leg_sources {
        let exact_match = sources.iter().any(|src| src == &rtp_addr);
        let port_match = sources
            .iter()
            .filter_map(|src| media_port(src))
            .any(|port| port == rtp_port);

        if exact_match || port_match {
            if matched_leg.is_some_and(|current_leg| current_leg != *leg) {
                return None;
            }
            matched_leg = Some(*leg);
        }
    }

    matched_leg
}

pub(crate) fn build_payload_type_map_by_leg(
    flow_items: &[SipFlowItem],
    leg_sources: &HashMap<i32, Vec<String>>,
) -> LegPayloadTypeMap {
    let mut maps = HashMap::new();

    for item in flow_items {
        if item.msg_type != SipFlowMsgType::Sip {
            continue;
        }

        let Ok(message) = std::str::from_utf8(&item.payload) else {
            continue;
        };
        let Some(sdp) = extract_sdp(message) else {
            continue;
        };
        let Some(leg) = infer_leg_for_sdp(&sdp, leg_sources) else {
            continue;
        };

        let map = maps.entry(leg).or_insert_with(HashMap::new);
        for info in MediaNegotiator::extract_all_codecs(&sdp) {
            map.insert(
                info.payload_type,
                PayloadDescriptor {
                    codec: info.codec,
                    clock_rate: info.clock_rate,
                },
            );
        }
    }

    maps
}

fn payload_descriptor(
    pt: u8,
    leg: i32,
    payload_map: &PayloadTypeMap,
    leg_payload_map: &LegPayloadTypeMap,
) -> PayloadDescriptor {
    leg_payload_map
        .get(&leg)
        .and_then(|map| map.get(&pt))
        .copied()
        .or_else(|| payload_map.get(&pt).copied())
        .unwrap_or_else(|| default_payload_descriptor(pt))
}

fn parse_dtmf_payload(payload: &[u8], clock_rate: u32) -> Option<(char, u32)> {
    if payload.len() < 4 || clock_rate == 0 {
        return None;
    }

    let digit = match payload[0] {
        0..=9 => (b'0' + payload[0]) as char,
        10 => '*',
        11 => '#',
        12..=15 => (b'A' + (payload[0] - 12)) as char,
        _ => return None,
    };

    let end_bit = (payload[1] & 0x80) != 0;
    if !end_bit {
        return None;
    }

    let duration = u16::from_be_bytes([payload[2], payload[3]]);
    let duration_ms = ((duration as u64 * 1000) / clock_rate as u64) as u32;
    Some((digit, duration_ms.max(20)))
}

fn looks_like_dtmf_payload(payload: &[u8]) -> bool {
    if payload.len() != 4 {
        return false;
    }

    matches!(payload[0], 0..=15) && (payload[1] & 0x40) == 0
}

fn encode_dtmf_tone(
    digit: char,
    duration_ms: u32,
    target_codec: Option<CodecType>,
    target_sample_rate: u32,
) -> Vec<u8> {
    let generator = DtmfGenerator::new(target_sample_rate);
    let pcm = generator.generate(digit, duration_ms);

    match target_codec {
        Some(codec) => create_encoder(codec).encode(&pcm),
        None => audio_codec::samples_to_bytes(&pcm),
    }
}

fn silence_chunk(target_codec: Option<CodecType>, step_samples: u32) -> Vec<u8> {
    match target_codec {
        Some(CodecType::PCMU) => vec![0x7F; step_samples as usize],
        Some(CodecType::PCMA) => vec![0xD5; step_samples as usize],
        None => vec![0u8; (step_samples * 2) as usize],
        _ => vec![0u8; step_samples as usize],
    }
}

fn insert_chunked(
    buffer: &mut BTreeMap<u32, Vec<u8>>,
    start_ts: u32,
    step_samples: u32,
    bytes_per_chunk: usize,
    silence_frame: &[u8],
    data: Vec<u8>,
) {
    for (idx, chunk) in data.chunks(bytes_per_chunk).enumerate() {
        let ts = start_ts + step_samples * idx as u32;
        let mut padded = silence_frame[..bytes_per_chunk].to_vec();
        padded[..chunk.len()].copy_from_slice(chunk);
        buffer.insert(ts, padded);
    }
}

fn mix_pcm_chunks(audio_chunk: &[u8], dtmf_chunk: &[u8]) -> Vec<u8> {
    let sample_count = audio_chunk.len().min(dtmf_chunk.len()) / 2;
    let mut mixed = Vec::with_capacity(sample_count * 2);

    for i in 0..sample_count {
        let offset = i * 2;
        let audio = i16::from_le_bytes([audio_chunk[offset], audio_chunk[offset + 1]]);
        let dtmf = i16::from_le_bytes([dtmf_chunk[offset], dtmf_chunk[offset + 1]]);
        let sum = audio as i32 + dtmf as i32;
        let clamped = sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        mixed.extend_from_slice(&clamped.to_le_bytes());
    }

    mixed
}

fn compose_leg_chunk(
    audio_chunk: Option<&Vec<u8>>,
    dtmf_chunk: Option<&Vec<u8>>,
    silence_frame: &[u8],
    target_codec: Option<CodecType>,
) -> Vec<u8> {
    match (audio_chunk, dtmf_chunk) {
        (Some(audio), Some(dtmf)) if target_codec.is_none() => mix_pcm_chunks(audio, dtmf),
        (Some(_audio), Some(dtmf)) => dtmf.clone(),
        (Some(audio), None) => audio.clone(),
        (None, Some(dtmf)) => dtmf.clone(),
        (None, None) => silence_frame.to_vec(),
    }
}

pub fn write_wav_header<W: Write + Seek>(
    writer: &mut W,
    codec: Option<CodecType>,
    sample_rate: u32,
    channels: u16,
    data_size: u32,
) -> Result<()> {
    writer.seek(SeekFrom::Start(0))?;

    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    let file_size = 36 + data_size;
    header[4..8].copy_from_slice(&file_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size

    let format_tag: u16 = match codec {
        Some(CodecType::PCMU) => 7,      // mu-law
        Some(CodecType::PCMA) => 6,      // a-law
        Some(CodecType::G722) => 0x0065, // G.722
        Some(CodecType::G729) => 0x0083, // G.729
        None => 1,                       // PCM
        _ => 1,                          // Default to PCM
    };

    header[20..22].copy_from_slice(&format_tag.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());

    let (bits_per_sample, byte_rate, block_align) = match codec {
        Some(CodecType::PCMU) | Some(CodecType::PCMA) => {
            let bps = 8;
            let br = sample_rate * channels as u32 * (bps as u32 / 8);
            let ba = channels * (bps / 8);
            (bps, br, ba)
        }
        Some(CodecType::G722) => {
            let bps = 0; // Often 0 for compressed
            let br = 8000 * channels as u32; // 64kbps per channel
            let ba = channels;
            (bps, br, ba)
        }
        _ => {
            let bps = 16;
            let br = sample_rate * channels as u32 * (bps as u32 / 8);
            let ba = channels * (bps / 8);
            (bps, br, ba)
        }
    };

    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());

    writer.write_all(&header)?;
    Ok(())
}

pub fn generate_wav_from_packets(packets: &[(i32, u64, Vec<u8>)]) -> Result<Vec<u8>> {
    generate_wav_from_packets_with_map_ex(packets, &HashMap::new(), false)
}

pub fn generate_wav_from_packets_ex(
    packets: &[(i32, u64, Vec<u8>)],
    force_pcm: bool,
) -> Result<Vec<u8>> {
    generate_wav_from_packets_with_map_ex(packets, &HashMap::new(), force_pcm)
}

pub(crate) fn generate_wav_from_packets_with_map_ex(
    packets: &[(i32, u64, Vec<u8>)],
    payload_map: &PayloadTypeMap,
    force_pcm: bool,
) -> Result<Vec<u8>> {
    generate_wav_from_packets_with_leg_map_ex(packets, payload_map, &HashMap::new(), force_pcm)
}

pub(crate) fn generate_wav_from_packets_with_leg_map_ex(
    packets: &[(i32, u64, Vec<u8>)],
    payload_map: &PayloadTypeMap,
    leg_payload_map: &LegPayloadTypeMap,
    force_pcm: bool,
) -> Result<Vec<u8>> {
    if packets.is_empty() {
        return Err(anyhow!("No RTP packets found"));
    }

    // 1. Analyze codecs to determine output format
    let mut legs_codecs: HashMap<i32, Vec<CodecType>> = HashMap::new();
    let mut leg_audio_clock_rates: HashMap<i32, u32> = HashMap::new();
    let mut min_ts = u64::MAX;
    let mut max_ts = 0;

    for (leg, ts, p) in packets {
        if p.len() < 12 {
            continue;
        }
        if *ts < min_ts {
            min_ts = *ts;
        }
        if *ts > max_ts {
            max_ts = *ts;
        }

        // Detect if it's a valid RTP v2 packet
        let pt = RtpPacket::parse(p)
            .map(|packet| packet.header.payload_type)
            .unwrap_or(0);
        let payload = RtpPacket::parse(p)
            .map(|packet| packet.payload.to_vec())
            .unwrap_or_default();

        if looks_like_dtmf_payload(&payload) {
            continue;
        }

        let codec = payload_descriptor(pt, *leg, payload_map, leg_payload_map);
        let codec_type = codec.codec;
        leg_audio_clock_rates
            .entry(*leg)
            .or_insert(codec.clock_rate);
        legs_codecs.entry(*leg).or_default().push(codec_type);
    }

    // Determine target codec
    let has_other = legs_codecs.values().any(|s| {
        s.iter()
            .any(|c| *c != CodecType::PCMU && *c != CodecType::PCMA)
    });
    let has_pcmu = legs_codecs
        .values()
        .any(|s| s.contains(&CodecType::PCMU));
    let has_pcma = legs_codecs
        .values()
        .any(|s| s.contains(&CodecType::PCMA));

    // If force_pcm is true, always output PCM format (for ASR compatibility)
    let (target_codec, target_sample_rate) = if force_pcm {
        (None, 16000)
    } else if !has_other && has_pcmu && !has_pcma {
        (Some(CodecType::PCMU), 8000)
    } else if !has_other && has_pcma && !has_pcmu {
        (Some(CodecType::PCMA), 8000)
    } else {
        (None, 16000)
    };

    tracing::info!(
        "Media export: target_codec={:?} rate={}",
        target_codec,
        target_sample_rate
    );
    let ptime_ms = 20;
    let step_samples = target_sample_rate * ptime_ms / 1000 ;
    let bytes_per_chunk = match target_codec {
        Some(CodecType::PCMU) | Some(CodecType::PCMA) => step_samples as usize,
        None => (step_samples * 2) as usize,
        _ => step_samples as usize,
    };
    let silence_frame = silence_chunk(target_codec, step_samples);

    // 2. Process streams
    let mut buffer_a: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut buffer_b: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut dtmf_buffer_a: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut dtmf_buffer_b: BTreeMap<u32, Vec<u8>> = BTreeMap::new();

    // Decoding State
    let mut decoders: HashMap<(i32, u8), Box<dyn Decoder>> = HashMap::new();
    let mut resamplers: HashMap<(i32, u8), Resampler> = HashMap::new();
    let mut base_timestamps: HashMap<i32, u64> = HashMap::new();

    for (leg, ts, p) in packets {
        if p.len() < 12 {
            continue;
        }

        let rtp = match rustrtc::rtp::RtpPacket::parse(p) {
            Ok(packet) => packet,
            Err(_) => {
                continue;
            }
        };

        let pt = rtp.header.payload_type;
        let payload = &rtp.payload;

        let mut descriptor = payload_descriptor(pt, *leg, payload_map, leg_payload_map);
        if descriptor.codec != CodecType::TelephoneEvent && looks_like_dtmf_payload(payload) {
            descriptor = PayloadDescriptor {
                codec: CodecType::TelephoneEvent,
                clock_rate: leg_audio_clock_rates.get(leg).copied().unwrap_or(8000),
            };
        }
        let codec = descriptor.codec;
        let clock_rate = descriptor.clock_rate as u64;

        let base = *base_timestamps.entry(*leg).or_insert(*ts);
        let rtp_diff = ts.wrapping_sub(base);
        let target_timestamp = (rtp_diff * target_sample_rate as u64 / clock_rate) as u32;

        if codec == CodecType::TelephoneEvent {
            if let Some((digit, duration_ms)) = parse_dtmf_payload(payload, descriptor.clock_rate) {
                let dtmf_audio =
                    encode_dtmf_tone(digit, duration_ms, target_codec, target_sample_rate);
                if *leg == 1 {
                    insert_chunked(
                        &mut dtmf_buffer_b,
                        target_timestamp,
                        step_samples,
                        bytes_per_chunk,
                        &silence_frame,
                        dtmf_audio,
                    );
                } else {
                    insert_chunked(
                        &mut dtmf_buffer_a,
                        target_timestamp,
                        step_samples,
                        bytes_per_chunk,
                        &silence_frame,
                        dtmf_audio,
                    );
                }
            }
            continue;
        }

        // Decoding logic...
        let decoder_needed = target_codec.is_none();

        // ... (existing decoding code)
        let processed_data: Vec<u8> = if decoder_needed {
            let decoder = decoders
                .entry((*leg, pt))
                .or_insert_with(|| create_decoder(codec));
            let samples = decoder.decode(payload);

            let current_rate = decoder.sample_rate();
            let final_samples = if current_rate != target_sample_rate {
                let resampler = resamplers.entry((*leg, pt)).or_insert_with(|| {
                    Resampler::new(current_rate as usize, target_sample_rate as usize)
                });
                resampler.resample(&samples)
            } else {
                samples
            };

            // Target is L16 (None)
            audio_codec::samples_to_bytes(&final_samples)
        } else {
            payload.to_vec()
        };

        if *leg == 1 {
            buffer_b.insert(target_timestamp, processed_data);
        } else {
            buffer_a.insert(target_timestamp, processed_data);
        }
    }

    // 3. Write WAV
    let mut cursor = Cursor::new(Vec::new());
    let mut writer = WavWriter::new_with_writer(&mut cursor, target_sample_rate, 2, target_codec);
    writer.write_header()?;

    let max_time_a = buffer_a.keys().max().cloned().unwrap_or(0);
    let max_time_b = buffer_b.keys().max().cloned().unwrap_or(0);
    let max_dtmf_time_a = dtmf_buffer_a.keys().max().cloned().unwrap_or(0);
    let max_dtmf_time_b = dtmf_buffer_b.keys().max().cloned().unwrap_or(0);
    let max_time = max_time_a
        .max(max_time_b)
        .max(max_dtmf_time_a)
        .max(max_dtmf_time_b);
    let max_duration = max_time + target_sample_rate / 50;

    tracing::info!(
        "Buffer stats: A_len={} B_len={} A_dtmf_len={} B_dtmf_len={} max_time={} max_duration={}",
        buffer_a.len(),
        buffer_b.len(),
        dtmf_buffer_a.len(),
        dtmf_buffer_b.len(),
        max_time,
        max_duration
    );

    let mut current_ts = 0;
    let mut silence_count = 0;
    let mut chunk_count = 0;

    while current_ts < max_duration {
        let audio_a = find_chunk(&buffer_a, current_ts, step_samples);
        let dtmf_a = find_chunk(&dtmf_buffer_a, current_ts, step_samples);
        let chunk_a = compose_leg_chunk(audio_a, dtmf_a, &silence_frame, target_codec);
        if chunk_a == silence_frame {
            silence_count += 1;
        } else {
            chunk_count += 1;
        }

        let audio_b = find_chunk(&buffer_b, current_ts, step_samples);
        let dtmf_b = find_chunk(&dtmf_buffer_b, current_ts, step_samples);
        let chunk_b = compose_leg_chunk(audio_b, dtmf_b, &silence_frame, target_codec);

        let mut interleaved = Vec::with_capacity(chunk_a.len() + chunk_b.len());

        if target_codec.is_none() {
            let count = chunk_a.len().min(chunk_b.len()) / 2;
            for i in 0..count {
                interleaved.extend_from_slice(&chunk_a[i * 2..(i + 1) * 2]);
                interleaved.extend_from_slice(&chunk_b[i * 2..(i + 1) * 2]);
            }
        } else {
            let count = chunk_a.len().min(chunk_b.len());
            for i in 0..count {
                interleaved.push(chunk_a[i]);
                interleaved.push(chunk_b[i]);
            }
        }

        writer.write_packet(&interleaved, 0)?;
        current_ts += step_samples;
    }
    tracing::info!(
        "Wav Gen: chunks={} silences={} total_steps={}",
        chunk_count,
        silence_count,
        current_ts / step_samples
    );

    writer.finalize()?;
    Ok(cursor.into_inner())
}

fn find_chunk(buffer: &BTreeMap<u32, Vec<u8>>, ts: u32, step: u32) -> Option<&Vec<u8>> {
    let tolerance = step / 2; // tighter tolerance for RTP sequence
    let start = ts.saturating_sub(tolerance);
    let end = ts + tolerance;

    // Find absolute closest match in the window
    buffer
        .range(start..end)
        .min_by_key(|(k, _)| (**k as i64 - ts as i64).abs())
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn build_rtp_packet(payload_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0u8; 12];
        packet[0] = 0x80;
        packet[1] = payload_type & 0x7f;
        packet.extend_from_slice(payload);
        packet
    }

    fn left_channel_pcm_samples(wav: &[u8]) -> Vec<i16> {
        wav[44..]
            .chunks_exact(4)
            .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
            .collect()
    }

    fn right_channel_pcm_samples(wav: &[u8]) -> Vec<i16> {
        wav[44..]
            .chunks_exact(4)
            .map(|frame| i16::from_le_bytes([frame[2], frame[3]]))
            .collect()
    }

    #[test]
    fn test_build_payload_type_map_extracts_telephone_event() {
        let item = SipFlowItem {
            timestamp: 0,
            seq: 0,
            msg_type: SipFlowMsgType::Sip,
            src_addr: String::new(),
            dst_addr: String::new(),
            payload: Bytes::from_static(
                b"INVITE sip:test@example.com SIP/2.0\r\n\r\nv=0\r\no=22-alice 1403 3692 IN IP4 127.0.0.1\r\ns=Talk\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 55785 RTP/AVP 9 101\r\na=rtpmap:101 telephone-event/8000\r\n",
            ),
        };

        let map = build_payload_type_map(&[item]);
        let descriptor = map
            .get(&101)
            .expect("telephone-event payload should be parsed");
        assert_eq!(descriptor.codec, CodecType::TelephoneEvent);
        assert_eq!(descriptor.clock_rate, 8000);
    }

    #[test]
    fn test_build_payload_type_map_by_leg_matches_media_ports() {
        let leg_a = SipFlowItem {
            timestamp: 0,
            seq: 0,
            msg_type: SipFlowMsgType::Sip,
            src_addr: String::new(),
            dst_addr: String::new(),
            payload: Bytes::from_static(
                b"INVITE sip:a@example.com SIP/2.0\r\n\r\nv=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.10\r\nt=0 0\r\nm=audio 5004 RTP/AVP 96 101\r\na=rtpmap:96 opus/48000/2\r\na=rtpmap:101 telephone-event/48000\r\n",
            ),
        };
        let leg_b = SipFlowItem {
            timestamp: 1,
            seq: 0,
            msg_type: SipFlowMsgType::Sip,
            src_addr: String::new(),
            dst_addr: String::new(),
            payload: Bytes::from_static(
                b"SIP/2.0 200 OK\r\n\r\nv=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.20\r\nt=0 0\r\nm=audio 4008 RTP/AVP 0 97\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:97 telephone-event/8000\r\n",
            ),
        };
        let leg_sources = HashMap::from([
            (0, vec!["10.0.0.10:5004".to_string()]),
            (1, vec!["10.0.0.20:4008".to_string()]),
        ]);

        let maps = build_payload_type_map_by_leg(&[leg_a, leg_b], &leg_sources);

        assert_eq!(
            maps.get(&0).and_then(|m| m.get(&101)).map(|d| d.clock_rate),
            Some(48000)
        );
        assert_eq!(
            maps.get(&1).and_then(|m| m.get(&97)).map(|d| d.clock_rate),
            Some(8000)
        );
    }

    #[test]
    fn test_generate_wav_from_packets_regenerates_telephone_event_without_stretching() {
        let mut payload_map = HashMap::new();
        payload_map.insert(
            101,
            PayloadDescriptor {
                codec: CodecType::TelephoneEvent,
                clock_rate: 48000,
            },
        );

        let packets = vec![(
            0,
            48_000,
            build_rtp_packet(101, &[5, 0x80, 0x03, 0xC0]), // digit 5, 20 ms at 48 kHz
        )];

        let wav = generate_wav_from_packets_with_map_ex(&packets, &payload_map, true)
            .expect("wav generation should succeed");

        assert!(
            wav.len() < 100_000,
            "DTMF export should not be stretched to several seconds"
        );
        assert!(
            wav.iter().skip(44).any(|b| *b != 0),
            "Regenerated DTMF should produce audible non-silent samples"
        );
    }

    #[test]
    fn test_generate_wav_keeps_full_dtmf_when_audio_resumes_on_same_leg() {
        let mut payload_map = HashMap::new();
        payload_map.insert(
            0,
            PayloadDescriptor {
                codec: CodecType::PCMU,
                clock_rate: 8000,
            },
        );
        payload_map.insert(
            101,
            PayloadDescriptor {
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
            },
        );

        let silence_payload = create_encoder(CodecType::PCMU).encode(&vec![0i16; 160]);
        let packets = vec![
            (
                0,
                0,
                build_rtp_packet(101, &[5, 0x80, 0x02, 0x80]), // 80 ms digit 5 at 8 kHz
            ),
            (0, 160, build_rtp_packet(0, &silence_payload)),
            (0, 320, build_rtp_packet(0, &silence_payload)),
            (0, 480, build_rtp_packet(0, &silence_payload)),
        ];

        let wav = generate_wav_from_packets_with_map_ex(&packets, &payload_map, true)
            .expect("wav generation should succeed");
        let left = left_channel_pcm_samples(&wav);

        for (index, window) in left.chunks(320).take(4).enumerate() {
            assert!(
                window.iter().any(|sample| *sample != 0),
                "DTMF window {} should remain audible after later same-leg audio packets",
                index
            );
        }
    }

    #[test]
    fn test_generate_wav_detects_unmapped_rfc4733_payloads() {
        let mut payload_map = HashMap::new();
        payload_map.insert(
            0,
            PayloadDescriptor {
                codec: CodecType::PCMU,
                clock_rate: 8000,
            },
        );

        let silence_payload = create_encoder(CodecType::PCMU).encode(&vec![0i16; 160]);
        let packets = vec![
            (1, 0, build_rtp_packet(0, &silence_payload)),
            (
                1,
                160,
                build_rtp_packet(97, &[5, 0x80, 0x06, 0x40]), // 200 ms digit 5 at 8 kHz
            ),
        ];

        let wav = generate_wav_from_packets_with_map_ex(&packets, &payload_map, true)
            .expect("wav generation should succeed");
        let right = right_channel_pcm_samples(&wav);
        let audible_windows = right
            .chunks(320)
            .filter(|window| window.iter().any(|sample| *sample != 0))
            .count();

        assert!(
            audible_windows >= 8,
            "RFC4733 payload without SDP mapping should still regenerate an audible tone, got {} windows",
            audible_windows
        );
    }
}
