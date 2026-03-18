use crate::media::StreamWriter;
use crate::media::wav_writer::WavWriter;
use anyhow::Result;
use audio_codec::{CodecType, Decoder, Encoder, create_decoder, create_encoder};
use bytes::Bytes;
use rustrtc::media::MediaSample;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;
use tracing::debug;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct RecorderOption {
    #[serde(default)]
    pub recorder_file: String,
    #[serde(default)]
    pub samplerate: u32,
    /// Flush interval in milliseconds (how often buffered audio is written to disk).
    /// Default 1280ms = LCM(4096-byte disk block, 160-sample codec ptime).
    /// Named "ptime" for config backward compatibility; not related to RTP ptime.
    #[serde(default)]
    pub ptime: u32,
}

impl RecorderOption {
    pub fn new(recorder_file: String) -> Self {
        Self {
            recorder_file,
            ..Default::default()
        }
    }
}

impl Default for RecorderOption {
    fn default() -> Self {
        Self {
            recorder_file: "".to_string(),
            samplerate: 8000,
            ptime: 1280,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Leg {
    A,
    B,
}

#[derive(Clone, Debug)]
struct DtmfEvent {
    digit: char,
    digit_code: u8,
    start_ts: u32,
    end_ts: u32,
    ended: bool,
}

pub struct Recorder {
    pub path: String,
    pub codec: CodecType,
    sample_rate: u32,
    dtmf_gen: DtmfGenerator,
    encoder: Option<Box<dyn Encoder>>,

    // Dynamic decoders and resamplers per leg and payload type
    decoders: HashMap<(Leg, u8), Box<dyn Decoder>>,
    resamplers: HashMap<(Leg, u8), audio_codec::Resampler>,

    // Buffers store encoded data indexed by absolute recording timestamp (in samples)
    buffer_a: BTreeMap<u32, Bytes>,
    buffer_b: BTreeMap<u32, Bytes>,

    start_instant: Instant,

    base_timestamp_a: Option<u32>,
    base_timestamp_b: Option<u32>,
    start_offset_a: u32, // in samples
    start_offset_b: u32, // in samples
    last_ssrc_a: Option<u32>,
    last_ssrc_b: Option<u32>,
    dtmf_events_a: Vec<DtmfEvent>,
    dtmf_events_b: Vec<DtmfEvent>,

    next_flush_ts: u32, // The next timestamp to be flushed
    ptime_samples: u32, // samples per flush period (e.g. 160 for 20ms @ 8kHz)

    written_samples: u64,
    writer: Box<dyn StreamWriter>,
}

/// Returns (samples_per_block, bytes_per_block) for the given codec.
fn codec_block_info(codec: CodecType) -> (u32, usize) {
    match codec {
        CodecType::G729 => (80, 10),
        CodecType::PCMU | CodecType::PCMA => (1, 1),
        CodecType::G722 => (2, 1),
        CodecType::Opus => (1, 2),
        _ => (1, 2),
    }
}

impl Recorder {
    pub fn new(path: &str, codec: CodecType) -> Result<Self> {
        Self::with_options(path, codec, 1280, 0)
    }

    pub fn with_options(path: &str, codec: CodecType, ptime_ms: u32, samplerate: u32) -> Result<Self> {
        // ensure the directory exists
        if let Some(parent) = PathBuf::from(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file = File::create(path)
            .map_err(|e| anyhow::anyhow!("Failed to create recorder file {}: {}", path, e))?;

        let src_codec = codec;
        let codec = match codec {
            CodecType::Opus => CodecType::PCMU,
            CodecType::G722 => CodecType::PCMU,
            _ => codec,
        };

        let sample_rate = if samplerate > 0 { samplerate } else { codec.samplerate() };
        let encoder = Some(create_encoder(codec));
        debug!(
            "Creating recorder: path={}, src_codec={:?} codec={:?} sample_rate={}",
            path, src_codec, codec, sample_rate
        );
        // PCMU/PCMA are mono codecs (1 channel), we force stereo (2 channels) for better separation
        // of leg A and leg B audio in the recording.
        let channels = 2;
        let mut writer = Box::new(WavWriter::new(file, sample_rate, channels, Some(codec)));
        writer.write_header()?;

        // Compute ptime_samples: flush interval normalized to multiple of codec ptime (20ms)
        let (samples_per_block, _) = codec_block_info(codec);
        let codec_ptime_samples = (sample_rate / 50 / samples_per_block) * samples_per_block;
        let codec_ptime_samples = codec_ptime_samples.max(samples_per_block);
        let ptime_ms = ptime_ms.max(20); // at least 20ms
        let raw_flush_samples = sample_rate * ptime_ms / 1000;
        let flush_periods = (raw_flush_samples / codec_ptime_samples).max(1);
        let ptime_samples = codec_ptime_samples * flush_periods;

        Ok(Self {
            path: path.to_string(),
            codec,
            sample_rate,
            dtmf_gen: DtmfGenerator::new(sample_rate),
            encoder,
            decoders: HashMap::new(),
            resamplers: HashMap::new(),
            buffer_a: BTreeMap::new(),
            buffer_b: BTreeMap::new(),
            start_instant: Instant::now(),
            base_timestamp_a: None,
            base_timestamp_b: None,
            start_offset_a: 0,
            start_offset_b: 0,
            last_ssrc_a: None,
            last_ssrc_b: None,
            dtmf_events_a: Vec::new(),
            dtmf_events_b: Vec::new(),
            next_flush_ts: 0,
            written_samples: 0,
            writer,
            ptime_samples,
        })
    }

    pub fn write_sample(
        &mut self,
        leg: Leg,
        sample: &MediaSample,
        dtmf_pt: Option<u8>,
        dtmf_clock_rate: Option<u32>,
        codec_hint: Option<CodecType>,
    ) -> Result<()> {
        let frame = match sample {
            MediaSample::Audio(frame) => frame,
            _ => return Ok(()),
        };

        if let (Some(pt), Some(dpt)) = (frame.payload_type, dtmf_pt) {
            if pt == dpt {
                return self.write_dtmf_payload(
                    leg,
                    &frame.data,
                    frame.rtp_timestamp,
                    dtmf_clock_rate.unwrap_or(self.sample_rate),
                );
            }
        }

        let decoder_type = match codec_hint {
            Some(codec) => codec,
            None => CodecType::try_from(frame.payload_type.unwrap_or(0))?,
        };
        let packet_ssrc = frame.raw_packet.as_ref().map(|packet| packet.header.ssrc);
        let (mut encoded, frame_clock_rate) = match sample {
            MediaSample::Audio(frame) => match frame.raw_packet.as_ref() {
                Some(packet) => (
                    Bytes::copy_from_slice(&packet.payload),
                    decoder_type.clock_rate().max(1),
                ),
                None => (frame.data.clone(), frame.clock_rate.max(1)),
            },
            _ => return Ok(()),
        };

        if decoder_type != self.codec {
            let decoder = self
                .decoders
                .entry((leg, decoder_type.payload_type()))
                .or_insert_with(|| create_decoder(decoder_type));
            let pcm = decoder.decode(&encoded);
            let resampler = self
                .resamplers
                .entry((leg, decoder_type.payload_type()))
                .or_insert_with(|| {
                    audio_codec::Resampler::new(
                        decoder.sample_rate() as usize,
                        self.sample_rate as usize,
                    )
                });
            let pcm = resampler.resample(&pcm);
            encoded = if let Some(enc) = self.encoder.as_mut() {
                enc.encode(&pcm)
            } else {
                audio_codec::samples_to_bytes(&pcm)
            }
            .into();
        }
        self.maybe_reset_leg_timeline(
            leg,
            frame.rtp_timestamp,
            frame_clock_rate,
            packet_ssrc,
            &encoded,
        );
        let absolute_ts = match leg {
            Leg::A => {
                if self.base_timestamp_a.is_none() {
                    self.base_timestamp_a = Some(frame.rtp_timestamp);
                    self.start_offset_a = (self.start_instant.elapsed().as_millis() as u64
                        * self.sample_rate as u64
                        / 1000) as u32;
                    debug!(
                        "Recorder Leg A: base_timestamp={} offset={}",
                        frame.rtp_timestamp, self.start_offset_a
                    );
                }
                let relative = frame
                    .rtp_timestamp
                    .wrapping_sub(self.base_timestamp_a.unwrap());
                let scaled_relative =
                    (relative as u64 * self.sample_rate as u64 / frame_clock_rate as u64) as u32;
                self.start_offset_a.wrapping_add(scaled_relative)
            }
            Leg::B => {
                if self.base_timestamp_b.is_none() {
                    self.base_timestamp_b = Some(frame.rtp_timestamp);
                    self.start_offset_b = (self.start_instant.elapsed().as_millis() as u64
                        * self.sample_rate as u64
                        / 1000) as u32;
                    debug!(
                        "Recorder Leg B: base_timestamp={} offset={}",
                        frame.rtp_timestamp, self.start_offset_b
                    );
                }
                let relative = frame
                    .rtp_timestamp
                    .wrapping_sub(self.base_timestamp_b.unwrap());
                let scaled_relative =
                    (relative as u64 * self.sample_rate as u64 / frame_clock_rate as u64) as u32;
                self.start_offset_b.wrapping_add(scaled_relative)
            }
        };

        match leg {
            Leg::A => {
                self.buffer_a.insert(absolute_ts, encoded);
            }
            Leg::B => {
                self.buffer_b.insert(absolute_ts, encoded);
            }
        }
        // Hold back flushing while there are active (non-ended) DTMF events,
        // so their audio region stays in the buffer for overlay.
        let flush_limit = self.dtmf_flush_limit();
        while self.next_flush_ts + self.ptime_samples <= flush_limit
            && self.max_available_ts() >= self.next_flush_ts + self.ptime_samples
        {
            self.flush_one_period()?;
        }
        Ok(())
    }

    fn maybe_reset_leg_timeline(
        &mut self,
        leg: Leg,
        rtp_timestamp: u32,
        frame_clock_rate: u32,
        packet_ssrc: Option<u32>,
        encoded: &[u8],
    ) {
        let leg_end = self.leg_end_ts(leg);
        let (base_timestamp, start_offset, last_ssrc) = match leg {
            Leg::A => (
                &mut self.base_timestamp_a,
                &mut self.start_offset_a,
                &mut self.last_ssrc_a,
            ),
            Leg::B => (
                &mut self.base_timestamp_b,
                &mut self.start_offset_b,
                &mut self.last_ssrc_b,
            ),
        };

        let Some(base) = *base_timestamp else {
            *last_ssrc = packet_ssrc;
            return;
        };

        let projected = start_offset.wrapping_add(
            ((rtp_timestamp.wrapping_sub(base)) as u64 * self.sample_rate as u64
                / frame_clock_rate.max(1) as u64) as u32,
        );
        let max_gap = self.sample_rate * 2;
        let ssrc_changed =
            matches!((*last_ssrc, packet_ssrc), (Some(prev), Some(curr)) if prev != curr);
        let timestamp_far_ahead = projected > leg_end.saturating_add(max_gap);
        let timestamp_far_behind = leg_end > projected.saturating_add(max_gap);
        let prev_ssrc = *last_ssrc;

        if ssrc_changed || timestamp_far_ahead || timestamp_far_behind {
            debug!(
                recorder_path = %self.path,
                leg = ?leg,
                base_timestamp = base,
                rtp_timestamp,
                frame_clock_rate,
                prev_ssrc,
                packet_ssrc,
                leg_end,
                projected,
                max_gap,
                ssrc_changed,
                timestamp_far_ahead,
                timestamp_far_behind,
                encoded_len = encoded.len(),
                "Recorder timeline discontinuity detected, resetting leg base"
            );
            *base_timestamp = Some(rtp_timestamp);
            *start_offset = leg_end;
        }

        *last_ssrc = packet_ssrc.or(*last_ssrc);
    }

    fn leg_end_ts(&self, leg: Leg) -> u32 {
        let (samples_per_block, bytes_per_block) = self.block_info();
        let buffered_end = match leg {
            Leg::A => self.buffer_a.last_key_value(),
            Leg::B => self.buffer_b.last_key_value(),
        }
        .map(|(k, v)| k + (v.len() / bytes_per_block) as u32 * samples_per_block)
        .unwrap_or(self.next_flush_ts);

        buffered_end.max(self.next_flush_ts)
    }

    pub fn write_dtmf_payload(
        &mut self,
        leg: Leg,
        payload: &[u8],
        timestamp: u32,
        clock_rate: u32,
    ) -> Result<()> {
        if payload.len() < 4 {
            return Ok(());
        }
        let digit_code = payload[0];
        let digit = match digit_code {
            0..=9 => (b'0' + digit_code) as char,
            10 => '*',
            11 => '#',
            12..=15 => (b'A' + (digit_code - 12)) as char,
            _ => return Ok(()),
        };

        let end_bit = (payload[1] & 0x80) != 0;
        let duration = u16::from_be_bytes([payload[2], payload[3]]);
        let duration_ms = (duration as u32 * 1000) / clock_rate.max(1);

        let start_ts = self.absolute_leg_timestamp(leg, timestamp, clock_rate);
        let duration_samples = (duration_ms as u64 * self.sample_rate as u64 / 1000).max(1) as u32;
        let end_ts = start_ts.saturating_add(duration_samples);

        let events = match leg {
            Leg::A => &mut self.dtmf_events_a,
            Leg::B => &mut self.dtmf_events_b,
        };

        if let Some(event) = events
            .iter_mut()
            .find(|event| event.digit_code == digit_code && event.start_ts == start_ts)
        {
            if end_ts > event.end_ts {
                event.end_ts = end_ts;
            }
            event.ended |= end_bit;
        } else {
            events.push(DtmfEvent {
                digit,
                digit_code,
                start_ts,
                end_ts,
                ended: end_bit,
            });
            events.sort_by_key(|event| event.start_ts);
        }

        if end_bit {
            debug!(
                leg = ?leg,
                digit = %digit,
                duration_ms = %duration_ms,
                "Recording DTMF digit (end)"
            );
        }

        Ok(())
    }

    /// Returns the flush limit: if there are active (non-ended) DTMF events,
    /// don't flush past their start_ts so the overlay can be applied.
    /// If all events are ended (or none exist), returns u32::MAX (no limit).
    fn dtmf_flush_limit(&self) -> u32 {
        let earliest_active = self
            .dtmf_events_a
            .iter()
            .chain(self.dtmf_events_b.iter())
            .filter(|e| !e.ended)
            .map(|e| e.start_ts)
            .min();
        earliest_active.unwrap_or(u32::MAX)
    }

    /// Returns the maximum timestamp across both leg buffers and DTMF events.
    fn max_available_ts(&self) -> u32 {
        let (samples_per_block, bytes_per_block) = self.block_info();
        let buf_end = |buf: &BTreeMap<u32, Bytes>| {
            buf.last_key_value()
                .map(|(k, v)| k + (v.len() / bytes_per_block) as u32 * samples_per_block)
                .unwrap_or(0)
        };
        let max_audio = buf_end(&self.buffer_a).max(buf_end(&self.buffer_b));
        let max_dtmf_a = self.dtmf_events_a.last().map(|e| e.end_ts).unwrap_or(0);
        let max_dtmf_b = self.dtmf_events_b.last().map(|e| e.end_ts).unwrap_or(0);
        max_audio.max(max_dtmf_a).max(max_dtmf_b)
    }

    /// Flush exactly one ptime period (ptime_samples).
    fn flush_one_period(&mut self) -> Result<()> {
        self.flush_samples(self.ptime_samples)
    }

    /// Core flush logic: flush exactly `len` samples from next_flush_ts.
    fn flush_samples(&mut self, len: u32) -> Result<()> {
        if len == 0 {
            return Ok(());
        }

        let flush_start = self.next_flush_ts;
        let data_a = self.get_leg_data(Leg::A, len)?;
        let data_b = self.get_leg_data(Leg::B, len)?;

        // Stereo: only decode/re-encode when DTMF overlaps, then interleave encoded blocks
        let data_a = self.apply_dtmf_if_needed(Leg::A, flush_start, len, data_a);
        let data_b = self.apply_dtmf_if_needed(Leg::B, flush_start, len, data_b);
        let output = self.interleave_encoded(&data_a, &data_b);

        // Advance flush position and prune DTMF events AFTER overlay has been applied
        self.next_flush_ts += len;
        self.prune_dtmf_events(self.next_flush_ts);

        self.writer.write_packet(&output, len as usize)?;
        self.written_samples += len as u64;

        Ok(())
    }

    pub fn finalize(&mut self) -> Result<()> {
        // Flush all complete periods
        while self.max_available_ts() >= self.next_flush_ts + self.ptime_samples {
            self.flush_one_period()?;
        }
        // Flush any remaining partial period
        let remaining = self.max_available_ts().saturating_sub(self.next_flush_ts);
        let (samples_per_block, _) = self.block_info();
        let remaining_aligned = (remaining / samples_per_block) * samples_per_block;
        if remaining_aligned > 0 {
            self.flush_samples(remaining_aligned)?;
        }
        self.writer.finalize()?;
        Ok(())
    }

    fn block_info(&self) -> (u32, usize) {
        codec_block_info(self.codec)
    }

    fn get_silence_bytes(&mut self, _leg: Leg, blocks: usize) -> Result<Bytes> {
        let (samples_per_block, _) = self.block_info();
        let mut num_samples = (blocks as u32 * samples_per_block) as usize;

        // Safety cap: never allocate more than 10 seconds of silence in one go
        let max_samples = (self.sample_rate * 10) as usize;
        if num_samples > max_samples {
            num_samples = max_samples;
        }

        let silence_pcm = vec![0i16; num_samples];

        if let Some(enc) = self.encoder.as_mut() {
            Ok(enc.encode(&silence_pcm).into())
        } else {
            Ok(audio_codec::samples_to_bytes(&silence_pcm).into())
        }
    }

    fn get_leg_data(&mut self, leg: Leg, samples: u32) -> Result<Vec<u8>> {
        let (samples_per_block, bytes_per_block) = self.block_info();
        let num_blocks = samples / samples_per_block;
        let mut result = Vec::with_capacity((num_blocks * bytes_per_block as u32) as usize);

        let mut current_ts = self.next_flush_ts;
        let flush_to = self.next_flush_ts + samples;

        while current_ts < flush_to {
            let next_ts = match leg {
                Leg::A => self.buffer_a.first_key_value().map(|(k, _)| *k),
                Leg::B => self.buffer_b.first_key_value().map(|(k, _)| *k),
            };

            if let Some(ts) = next_ts {
                if ts < flush_to {
                    // Check for gap
                    if ts > current_ts {
                        let mut gap_samples = ts - current_ts;
                        let max_gap = self.sample_rate; // Max 1 second of silence
                        if gap_samples > max_gap {
                            debug!(
                                "Recorder gap too large: {} samples, capping to {}",
                                gap_samples, max_gap
                            );
                            gap_samples = max_gap;
                        }
                        let gap_blocks = gap_samples / samples_per_block;
                        result.extend(self.get_silence_bytes(leg, gap_blocks as usize)?);
                        current_ts = ts;
                    }

                    let data = match leg {
                        Leg::A => self.buffer_a.pop_first().unwrap().1,
                        Leg::B => self.buffer_b.pop_first().unwrap().1,
                    };
                    let data_samples = (data.len() / bytes_per_block) as u32 * samples_per_block;
                    if current_ts + data_samples > flush_to {
                        let keep_samples = flush_to - current_ts;
                        let keep_blocks = keep_samples / samples_per_block;
                        let keep_bytes = keep_blocks as usize * bytes_per_block;
                        if keep_bytes > 0 {
                            result.extend_from_slice(&data[..keep_bytes]);
                            let rest_bytes = &data[keep_bytes..];
                            if !rest_bytes.is_empty() {
                                let rest_ts = current_ts + (keep_blocks * samples_per_block);
                                match leg {
                                    Leg::A => self
                                        .buffer_a
                                        .insert(rest_ts, Bytes::copy_from_slice(rest_bytes)),
                                    Leg::B => self
                                        .buffer_b
                                        .insert(rest_ts, Bytes::copy_from_slice(rest_bytes)),
                                };
                            }
                            current_ts += keep_blocks * samples_per_block;
                        } else {
                            // Put it all back
                            match leg {
                                Leg::A => self.buffer_a.insert(ts, data),
                                Leg::B => self.buffer_b.insert(ts, data),
                            };
                            break;
                        }
                    } else {
                        result.extend_from_slice(&data);
                        current_ts += data_samples;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        // Fill remaining with silence
        if current_ts < flush_to {
            let gap_samples = flush_to - current_ts;
            let gap_blocks = gap_samples / samples_per_block;
            result.extend(self.get_silence_bytes(leg, gap_blocks as usize)?);
        }

        Ok(result)
    }

    fn absolute_leg_timestamp(
        &mut self,
        leg: Leg,
        timestamp: u32,
        timestamp_clock_rate: u32,
    ) -> u32 {
        match leg {
            Leg::A => {
                if self.base_timestamp_a.is_none() {
                    self.base_timestamp_a = Some(timestamp);
                    self.start_offset_a = (self.start_instant.elapsed().as_millis() as u64
                        * self.sample_rate as u64
                        / 1000) as u32;
                }
                let relative = timestamp.wrapping_sub(self.base_timestamp_a.unwrap());
                let scaled_relative = (relative as u64 * self.sample_rate as u64
                    / timestamp_clock_rate.max(1) as u64)
                    as u32;
                self.start_offset_a.wrapping_add(scaled_relative)
            }
            Leg::B => {
                if self.base_timestamp_b.is_none() {
                    self.base_timestamp_b = Some(timestamp);
                    self.start_offset_b = (self.start_instant.elapsed().as_millis() as u64
                        * self.sample_rate as u64
                        / 1000) as u32;
                }
                let relative = timestamp.wrapping_sub(self.base_timestamp_b.unwrap());
                let scaled_relative = (relative as u64 * self.sample_rate as u64
                    / timestamp_clock_rate.max(1) as u64)
                    as u32;
                self.start_offset_b.wrapping_add(scaled_relative)
            }
        }
    }

    /// Apply DTMF overlay directly onto PCM samples in-place.
    fn apply_dtmf_overlay_pcm(&self, leg: Leg, flush_start: u32, pcm: &mut [i16]) {
        let flush_len = pcm.len() as u32;
        let flush_end = flush_start + flush_len;
        let events = match leg {
            Leg::A => &self.dtmf_events_a,
            Leg::B => &self.dtmf_events_b,
        };

        for event in events {
            if event.end_ts <= flush_start || event.start_ts >= flush_end {
                continue;
            }

            let overlap_start = event.start_ts.max(flush_start);
            let overlap_end = event.end_ts.min(flush_end);
            let segment_start = overlap_start - event.start_ts;
            let segment_len = overlap_end - overlap_start;
            if segment_len == 0 {
                continue;
            }

            let total_duration = (event.end_ts - event.start_ts) as usize;
            let dtmf_pcm = self.dtmf_gen.generate_segment(
                event.digit,
                segment_start as usize,
                segment_len as usize,
            );
            // Apply fade-in/fade-out ramp (2ms) to avoid clicks at tone boundaries
            let ramp_samples = (self.sample_rate as usize / 500).max(1); // 2ms
            let pcm_offset = (overlap_start - flush_start) as usize;
            for (i, sample) in dtmf_pcm.iter().enumerate() {
                let abs_pos = segment_start as usize + i; // position within the full tone
                let mut gain = 1.0f32;
                // Fade in at tone start
                if abs_pos < ramp_samples {
                    gain = abs_pos as f32 / ramp_samples as f32;
                }
                // Fade out at tone end — only if the event has ended (we know the real end)
                if event.ended {
                    let from_end = total_duration.saturating_sub(abs_pos + 1);
                    if from_end < ramp_samples {
                        gain = gain.min((from_end + 1) as f32 / ramp_samples as f32);
                    }
                }
                let ramped = (*sample as f32 * gain) as i32;
                if let Some(dest) = pcm.get_mut(pcm_offset + i) {
                    *dest = ((*dest as i32 + ramped)
                        .clamp(i16::MIN as i32, i16::MAX as i32))
                        as i16;
                }
            }

            debug!(
                "Recording DTMF overlay: leg={:?}, digit={}, duration={}ms, segment_start={}ms, segment_len={}ms, flush_start={}, event_start={}",
                leg,
                event.digit,
                ((event.end_ts - event.start_ts) as u64 * 1000 / self.sample_rate as u64) as u32,
                (segment_start as u64 * 1000 / self.sample_rate as u64) as u32,
                (segment_len as u64 * 1000 / self.sample_rate as u64) as u32,
                flush_start,
                event.start_ts,
            );
        }
    }

    /// For stereo mode: decode, overlay DTMF, re-encode only when DTMF events overlap.
    /// Otherwise return the encoded data unchanged to avoid a wasteful codec round-trip.
    fn apply_dtmf_if_needed(&self, leg: Leg, flush_start: u32, flush_len: u32, data: Vec<u8>) -> Vec<u8> {
        if data.is_empty() {
            return data;
        }
        let flush_end = flush_start + flush_len;
        let events = match leg {
            Leg::A => &self.dtmf_events_a,
            Leg::B => &self.dtmf_events_b,
        };
        let has_overlap = events
            .iter()
            .any(|e| e.start_ts < flush_end && e.end_ts > flush_start);
        if !has_overlap {
            return data;
        }

        let mut pcm = create_decoder(self.codec).decode(&data);
        self.apply_dtmf_overlay_pcm(leg, flush_start, &mut pcm);
        create_encoder(self.codec).encode(&pcm)
    }

    fn prune_dtmf_events(&mut self, flushed_to: u32) {
        let stale_threshold = self.sample_rate * 2; // 2 seconds
        let retain = |event: &DtmfEvent| {
            if event.ended && event.end_ts <= flushed_to {
                return false; // fully flushed
            }
            // Prune stale non-ended events (end packet lost)
            if !event.ended && flushed_to > event.end_ts.saturating_add(stale_threshold) {
                return false;
            }
            true
        };
        self.dtmf_events_a.retain(retain);
        self.dtmf_events_b.retain(retain);
    }

    /// Interleave encoded audio blocks for stereo WAV output.
    fn interleave_encoded(&self, data_a: &[u8], data_b: &[u8]) -> Vec<u8> {
        let (_, bytes_per_block) = self.block_info();
        let len = data_a.len().min(data_b.len());
        let mut interleaved = Vec::with_capacity(len * 2);
        let num_blocks = len / bytes_per_block;
        for i in 0..num_blocks {
            interleaved.extend_from_slice(&data_a[i * bytes_per_block..(i + 1) * bytes_per_block]);
            interleaved.extend_from_slice(&data_b[i * bytes_per_block..(i + 1) * bytes_per_block]);
        }
        interleaved
    }

    /// Mix two PCM sample slices by averaging (prevents clipping).
    #[cfg(test)]
    fn mix_pcm(pcm_a: &[i16], pcm_b: &[i16]) -> Vec<i16> {
        let len = pcm_a.len().min(pcm_b.len());
        let mut mixed = Vec::with_capacity(len);
        for i in 0..len {
            mixed.push(((pcm_a[i] as i32 + pcm_b[i] as i32) / 2) as i16);
        }
        mixed
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.finalize();
    }
}

pub struct DtmfGenerator {
    sample_rate: u32,
}

impl DtmfGenerator {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    fn freqs(digit: char) -> Option<(f32, f32)> {
        match digit {
            '1' => Some((697.0, 1209.0)),
            '2' => Some((697.0, 1336.0)),
            '3' => Some((697.0, 1477.0)),
            '4' => Some((770.0, 1209.0)),
            '5' => Some((770.0, 1336.0)),
            '6' => Some((770.0, 1477.0)),
            '7' => Some((852.0, 1209.0)),
            '8' => Some((852.0, 1336.0)),
            '9' => Some((852.0, 1477.0)),
            '*' => Some((941.0, 1209.0)),
            '0' => Some((941.0, 1336.0)),
            '#' => Some((941.0, 1477.0)),
            'A' => Some((697.0, 1633.0)),
            'B' => Some((770.0, 1633.0)),
            'C' => Some((852.0, 1633.0)),
            'D' => Some((941.0, 1633.0)),
            _ => None,
        }
    }

    fn generate_samples(&self, freqs: (f32, f32), start_sample: usize, num_samples: usize) -> Vec<i16> {
        let mut samples = Vec::with_capacity(num_samples);
        for i in 0..num_samples {
            let t = (start_sample + i) as f32 / self.sample_rate as f32;
            let s1 = (2.0 * std::f32::consts::PI * freqs.0 * t).sin();
            let s2 = (2.0 * std::f32::consts::PI * freqs.1 * t).sin();
            let s = (s1 + s2) / 2.0;
            samples.push((s * 32767.0) as i16);
        }
        samples
    }

    pub fn generate(&self, digit: char, duration_ms: u32) -> Vec<i16> {
        let Some(freqs) = Self::freqs(digit) else {
            return Vec::new();
        };
        let num_samples = (self.sample_rate as f32 * (duration_ms as f32 / 1000.0)) as usize;
        self.generate_samples(freqs, 0, num_samples)
    }

    pub fn generate_segment(
        &self,
        digit: char,
        start_sample: usize,
        num_samples: usize,
    ) -> Vec<i16> {
        let Some(freqs) = Self::freqs(digit) else {
            return Vec::new();
        };
        self.generate_samples(freqs, start_sample, num_samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{CodecType, create_decoder, create_encoder};

    #[test]
    fn test_mix_pcm_both_silent() {
        let pcm_a = vec![0i16; 160];
        let pcm_b = vec![0i16; 160];
        let mixed = Recorder::mix_pcm(&pcm_a, &pcm_b);
        let max_sample = mixed.iter().map(|&s| s.abs()).max().unwrap_or(0);
        assert_eq!(max_sample, 0, "Mixed silent audio should remain silent");
    }

    #[test]
    fn test_progressive_dtmf_updates_do_not_duplicate_recorded_duration() {
        let mut recorder = create_test_recorder(CodecType::PCMU);
        // Pin the timeline to avoid timing-dependent start_offset
        recorder.base_timestamp_a = Some(0);
        recorder.start_offset_a = 0;

        // Feed audio data alongside DTMF to advance the sample-based clock.
        // Each iteration: push a 160-sample silence packet at the DTMF end_ts,
        // which gives the recorder enough data to potentially flush periods.
        let mut encoder = create_encoder(CodecType::PCMU);
        let silence = encoder.encode(&vec![0i16; 160]);

        for duration in [160u16, 320, 480, 640, 800, 960, 1120, 1280, 1440] {
            let payload = [5, 0x00, (duration >> 8) as u8, duration as u8];
            recorder
                .write_dtmf_payload(Leg::A, &payload, 0, 8000)
                .expect("progressive DTMF should be accepted");
            // Insert audio at the current DTMF end position to drive the flush clock
            let audio_ts = duration as u32;
            recorder.buffer_a.insert(audio_ts, Bytes::from(silence.clone()));
        }

        let payload = [5, 0x80, 0x06, 0x40];
        recorder
            .write_dtmf_payload(Leg::A, &payload, 0, 8000)
            .expect("terminal DTMF should be accepted");
        recorder.finalize().expect("finalize should succeed");

        assert_eq!(
            recorder.written_samples, 1600,
            "progressive RFC4733 updates should produce one 200ms tone, not accumulate duplicate writes"
        );
        assert!(recorder.dtmf_events_a.is_empty());
    }

    #[test]
    fn test_mix_pcm_one_silent() {
        let pcm_silent = vec![0i16; 160];
        let pcm_active: Vec<i16> = (0..160)
            .map(|i| ((i as f32 / 10.0).sin() * 5000.0) as i16)
            .collect();

        let mixed = Recorder::mix_pcm(&pcm_silent, &pcm_active);

        let avg_mixed: i32 = mixed
            .iter()
            .take(100)
            .map(|&s| s.abs() as i32)
            .sum::<i32>()
            / 100;
        let avg_original: i32 = pcm_active
            .iter()
            .take(100)
            .map(|&s| s.abs() as i32)
            .sum::<i32>()
            / 100;

        assert!(avg_original > 100, "Original signal should be non-zero, got {}", avg_original);
        assert!(avg_mixed > 50, "Mixed signal should be non-zero, got {}", avg_mixed);

        let ratio = avg_mixed as f32 / avg_original as f32;
        assert!(
            ratio > 0.45 && ratio < 0.55,
            "Mixed amplitude should be exactly 0.5x original in PCM domain, got ratio={} (mixed={}, orig={})",
            ratio, avg_mixed, avg_original
        );
    }

    #[test]
    fn test_mix_pcm_both_active() {
        let pcm_a: Vec<i16> = (0..160)
            .map(|i| (i as f32 * 50.0).sin() as i16 * 2000)
            .collect();
        let pcm_b: Vec<i16> = (0..160)
            .map(|i| (i as f32 * 70.0).sin() as i16 * 3000)
            .collect();

        let mixed = Recorder::mix_pcm(&pcm_a, &pcm_b);

        assert_eq!(mixed.len(), pcm_a.len());

        let sample_idx = 50;
        let expected = ((pcm_a[sample_idx] as i32 + pcm_b[sample_idx] as i32) / 2) as i16;
        let actual = mixed[sample_idx];
        assert_eq!(
            expected, actual,
            "Mixed sample should be exact average in PCM domain"
        );
    }

    #[test]
    fn test_interleave_encoded() {
        let recorder = create_test_recorder(CodecType::PCMU);

        // Create distinct encoded test data (PCMU: 1 byte per sample)
        let data_a: Vec<u8> = (0..160).map(|i| (i % 256) as u8).collect();
        let data_b: Vec<u8> = (0..160).map(|i| ((i + 128) % 256) as u8).collect();

        let interleaved = recorder.interleave_encoded(&data_a, &data_b);

        assert_eq!(interleaved.len(), data_a.len() + data_b.len());

        for i in 0..10 {
            assert_eq!(
                interleaved[i * 2],
                data_a[i],
                "Interleaved data should alternate: A at position {}",
                i * 2
            );
            assert_eq!(
                interleaved[i * 2 + 1],
                data_b[i],
                "Interleaved data should alternate: B at position {}",
                i * 2 + 1
            );
        }
    }

    #[test]
    fn test_mix_pcm_prevents_clipping() {
        let pcm_high = vec![20000i16; 160];
        let mixed = Recorder::mix_pcm(&pcm_high, &pcm_high);

        let avg_result: i32 =
            mixed.iter().map(|&s| s as i32).sum::<i32>() / mixed.len() as i32;
        assert_eq!(
            avg_result, 20000,
            "Mixed identical high-amplitude audio should average to same value"
        );
    }

    // Helper function to create a test recorder
    fn create_test_recorder(codec: CodecType) -> Recorder {
        let sample_rate = codec.samplerate();
        let encoder = Some(create_encoder(codec));
        let (samples_per_block, _) = codec_block_info(codec);
        let codec_ptime_samples = ((sample_rate / 50) / samples_per_block * samples_per_block).max(samples_per_block);
        let flush_periods = (sample_rate / codec_ptime_samples).max(1);
        let ptime_samples = codec_ptime_samples * flush_periods;

        Recorder {
            path: "test.wav".to_string(),
            codec,
            sample_rate,
            dtmf_gen: DtmfGenerator::new(sample_rate),
            encoder,
            decoders: HashMap::new(),
            resamplers: HashMap::new(),
            buffer_a: BTreeMap::new(),
            buffer_b: BTreeMap::new(),
            start_instant: Instant::now(),
            base_timestamp_a: None,
            base_timestamp_b: None,
            start_offset_a: 0,
            start_offset_b: 0,
            last_ssrc_a: None,
            last_ssrc_b: None,
            next_flush_ts: 0,
            written_samples: 0,
            writer: Box::new(TestWriter::new()),
            ptime_samples,
            dtmf_events_a: Vec::new(),
            dtmf_events_b: Vec::new(),
        }
    }

    // Mock writer for testing
    struct TestWriter {
        data: Vec<u8>,
    }

    impl TestWriter {
        fn new() -> Self {
            Self { data: Vec::new() }
        }
    }

    impl StreamWriter for TestWriter {
        fn write_header(&mut self) -> Result<()> {
            Ok(())
        }

        fn write_packet(&mut self, data: &[u8], _samples: usize) -> Result<()> {
            self.data.extend_from_slice(data);
            Ok(())
        }

        fn finalize(&mut self) -> Result<()> {
            Ok(())
        }
    }
}
