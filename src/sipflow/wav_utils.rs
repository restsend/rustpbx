use anyhow::{Result, anyhow};
use audio_codec::{CodecType, Decoder, Resampler, create_decoder};
use std::{
    collections::{BTreeMap, HashMap},
    io::{Cursor, Seek, SeekFrom, Write},
};

use crate::media::{StreamWriter, wav_writer::WavWriter};

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
            let ba = 1 * channels;
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
    if packets.is_empty() {
        return Err(anyhow!("No RTP packets found"));
    }

    // 1. Analyze codecs to determine output format
    let mut legs_codecs: HashMap<i32, Vec<CodecType>> = HashMap::new();
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
        let is_rtp = p.len() >= 12 && (p[0] & 0xC0) == 0x80;

        let pt = if is_rtp { p[1] & 0x7F } else { 0 }; // Default to PCMU if raw
        let codec = match pt {
            0 => CodecType::PCMU,
            8 => CodecType::PCMA,
            9 => CodecType::G722,
            18 => CodecType::G729,
            _ => CodecType::PCMU,
        };
        legs_codecs.entry(*leg).or_default().push(codec);
    }

    // Determine target codec
    let has_other = legs_codecs.values().any(|s| {
        s.iter()
            .any(|c| *c != CodecType::PCMU && *c != CodecType::PCMA)
    });
    let has_pcmu = legs_codecs
        .values()
        .any(|s| s.iter().any(|c| *c == CodecType::PCMU));
    let has_pcma = legs_codecs
        .values()
        .any(|s| s.iter().any(|c| *c == CodecType::PCMA));

    let (target_codec, target_sample_rate) = if !has_other && has_pcmu && !has_pcma {
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

    // 2. Process streams
    let mut buffer_a: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut buffer_b: BTreeMap<u32, Vec<u8>> = BTreeMap::new();

    // Decoding State
    let mut decoders: HashMap<(i32, u8), Box<dyn Decoder>> = HashMap::new();
    let mut resamplers: HashMap<(i32, u8), Resampler> = HashMap::new();
    let mut base_timestamps: HashMap<i32, u64> = HashMap::new();

    // Re-buffer logic
    let mut logged_packets = 0;
    for (leg, ts, p) in packets {
        if p.len() < 12 {
            continue;
        }

        let is_rtp = (p[0] & 0xC0) == 0x80;
        let pt = if is_rtp { p[1] & 0x7F } else { 0 };
        let payload = if is_rtp { &p[12..] } else { &p[..] };

        let codec = match pt {
            0 => CodecType::PCMU,
            8 => CodecType::PCMA,
            9 => CodecType::G722,
            18 => CodecType::G729,
            _ => CodecType::PCMU,
        };

        let base = *base_timestamps.entry(*leg).or_insert(*ts);

        // Decoding logic...
        let decoder_needed = target_codec.is_none();

        let processed_data: Vec<u8>;
        // ... (existing decoding code)
        if decoder_needed {
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
            processed_data = audio_codec::samples_to_bytes(&final_samples);
        } else {
            processed_data = payload.to_vec();
        }

        let rtp_diff = ts.wrapping_sub(base);
        let target_timestamp = (rtp_diff as u64 * target_sample_rate as u64 / 8000) as u32;

        if logged_packets < 20 {
            let header_hex = if p.len() >= 12 {
                hex::encode(&p[0..12])
            } else {
                "truncated".to_string()
            };

            let samples_have = if pt == 0 || pt == 8 {
                processed_data.len() // 1 byte per sample
            } else if pt == 9 {
                processed_data.len() / 2 // Decoded G722 is L16 (2 bytes)
            } else {
                processed_data.len() / 2
            };
            tracing::info!(
                "Packet: leg={} ts={} diff_us={} target_step={} p_len={} pt={} header={} samples_got={}",
                leg,
                ts,
                rtp_diff,
                target_timestamp,
                p.len(),
                pt,
                header_hex,
                samples_have
            );
            logged_packets += 1;
        }

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
    let max_time = std::cmp::max(max_time_a, max_time_b);
    let max_duration = max_time + target_sample_rate / 50;

    tracing::info!(
        "Buffer stats: A_len={} B_len={} max_time={} max_duration={}",
        buffer_a.len(),
        buffer_b.len(),
        max_time,
        max_duration
    );

    let ptime_ms = 20;
    let step_samples = (target_sample_rate * ptime_ms / 1000) as u32;
    // Silence frames
    let silence_frame = match target_codec {
        Some(CodecType::PCMU) => vec![0x7F; step_samples as usize],
        Some(CodecType::PCMA) => vec![0xD5; step_samples as usize],
        None => vec![0u8; (step_samples * 2) as usize], // PCM 16bit silence is 0
        _ => vec![0u8; step_samples as usize],
    };

    let mut current_ts = 0;
    let mut silence_count = 0;
    let mut chunk_count = 0;

    while current_ts < max_duration {
        let chunk_a = find_chunk(&buffer_a, current_ts, step_samples).unwrap_or_else(|| {
            // tracing::trace!("Leg A gap at {}", current_ts);
            silence_count += 1;
            &silence_frame
        });
        if !chunk_a.is_empty() && chunk_a != &silence_frame {
            chunk_count += 1;
        }

        let chunk_b =
            find_chunk(&buffer_b, current_ts, step_samples).unwrap_or_else(|| &silence_frame);

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
