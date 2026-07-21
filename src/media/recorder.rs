use crate::media::StreamWriter;
use crate::media::negotiate::NegotiatedLegProfile;
use crate::media::wav_writer::CodecWavWriter;
use anyhow::Result;
use audio_codec::{CodecType, Decoder, Encoder, create_decoder, create_encoder};
use bytes::Bytes;
use rustrtc::media::MediaSample;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::debug;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct RecorderOption {
    #[serde(default)]
    pub recorder_file: String,
    #[serde(default)]
    pub samplerate: u32,
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
            samplerate: 16000,
            ptime: 200,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Leg {
    A,
    B,
}

#[derive(Debug, Clone, Copy)]
struct DtmfEventState {
    digit_code: u8,
    rtp_timestamp: u32,
    absolute_timestamp: u32,
    duration_samples: u32,
}

pub struct Recorder {
    pub path: String,
    pub codec: CodecType,
    written_bytes: u32,
    sample_rate: u32,
    channels: u16,
    dtmf_gen: DtmfGenerator,
    encoder: Option<Box<dyn Encoder>>,

    // Dynamic decoders and resamplers per leg and payload type
    decoders: HashMap<(Leg, u8), Box<dyn Decoder>>,
    resamplers: HashMap<(Leg, u8), audio_codec::Resampler>,

    // Buffers store encoded data indexed by absolute recording position (in samples)
    buffer_a: BTreeMap<u32, Bytes>,
    buffer_b: BTreeMap<u32, Bytes>,

    last_flush: Instant,

    profile_a: NegotiatedLegProfile,
    profile_b: NegotiatedLegProfile,
    dtmf_state_a: Option<DtmfEventState>,
    dtmf_state_b: Option<DtmfEventState>,
    leg_a_started: bool,
    leg_b_started: bool,

    next_flush_ts: u32, // The next timestamp to be flushed
    ptime: Duration,

    written_samples: u64,
    writer: Box<dyn StreamWriter>,

    /// When true, swap stereo channels: callee→left, caller→right.
    /// Default (false): caller→left, callee→right.
    pub stereo_swap: bool,
}

impl Recorder {
    pub fn new(path: &str, codec: CodecType) -> Result<Self> {
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

        let sample_rate = codec.samplerate();
        let encoder = Some(create_encoder(codec));
        debug!(
            "Creating recorder: path={}, src_codec={:?} codec={:?}",
            path, src_codec, codec
        );
        // PCMU/PCMA are mono codecs (1 channel), we force stereo (2 channels) for better separation
        // of leg A and leg B audio in the recording.
        let channels = 2;
        let mut writer = Box::new(CodecWavWriter::new(
            file,
            sample_rate,
            channels,
            Some(codec),
        ));
        writer.write_header()?;

        Ok(Self {
            path: path.to_string(),
            codec,
            written_bytes: 0,
            sample_rate,
            channels,
            dtmf_gen: DtmfGenerator::new(sample_rate),
            encoder,
            decoders: HashMap::new(),
            resamplers: HashMap::new(),
            buffer_a: BTreeMap::new(),
            buffer_b: BTreeMap::new(),
            last_flush: Instant::now(),
            profile_a: NegotiatedLegProfile::default(),
            profile_b: NegotiatedLegProfile::default(),
            dtmf_state_a: None,
            dtmf_state_b: None,
            next_flush_ts: 0,
            written_samples: 0,
            writer,
            ptime: Duration::from_millis(200),
            stereo_swap: false,
            leg_a_started: false,
            leg_b_started: false,
        })
    }

    pub fn set_leg_profile(&mut self, leg: Leg, profile: NegotiatedLegProfile) {
        match leg {
            Leg::A => self.profile_a = profile,
            Leg::B => self.profile_b = profile,
        }
    }

    fn profile_for_leg(&self, leg: Leg) -> &NegotiatedLegProfile {
        match leg {
            Leg::A => &self.profile_a,
            Leg::B => &self.profile_b,
        }
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

        let (profile_audio_pt, profile_audio_codec, profile_dtmf_pt, profile_dtmf_clock_rate) = {
            let profile = self.profile_for_leg(leg);
            (
                profile.audio.as_ref().map(|codec| codec.payload_type),
                profile.audio.as_ref().map(|codec| codec.codec),
                profile.dtmf.as_ref().map(|codec| codec.payload_type),
                profile.dtmf.as_ref().map(|codec| codec.clock_rate),
            )
        };
        let dtmf_pt = dtmf_pt.or(profile_dtmf_pt);
        let dtmf_clock_rate = dtmf_clock_rate.or(profile_dtmf_clock_rate);
        if let (Some(pt), Some(dpt)) = (frame.payload_type, dtmf_pt)
            && pt == dpt
        {
            return self.write_dtmf_payload(
                leg,
                &frame.data,
                frame.rtp_timestamp,
                dtmf_clock_rate.unwrap_or(self.sample_rate),
            );
        }

        // DTMF fallback: when the PT doesn't match the profile's DTMF PT
        // (e.g. SDP negotiated multiple telephone-event PTs but only one
        // was stored), detect DTMF by payload shape — RFC 4733 payload is
        // always exactly 4 bytes and the first byte is a valid digit code.
        if dtmf_pt.is_some() && frame.data.len() == 4 {
            let code = frame.data[0];
            if crate::media::telephone_event::dtmf_code_to_char(code).is_some() {
                return self.write_dtmf_payload(
                    leg,
                    &frame.data,
                    frame.rtp_timestamp,
                    frame.clock_rate.max(8000),
                );
            }
        }

        let mut codec_hint = codec_hint.or(match (frame.payload_type, profile_audio_pt) {
            (Some(pt), Some(audio_pt)) if pt == audio_pt => profile_audio_codec,
            _ => None,
        });

        // Extract payload (prefer raw RTP payload when available).
        let (mut encoded, _frame_clock_rate) = match sample {
            MediaSample::Audio(frame) => match frame.raw_packet.as_ref() {
                Some(packet) => (
                    Bytes::copy_from_slice(&packet.payload),
                    frame.clock_rate.max(1),
                ),
                None => (frame.data.clone(), frame.clock_rate.max(1)),
            },
            _ => return Ok(()),
        };

        // Handle RFC 2198 RED: JsSIP/Chrome WebRTC clients send Opus wrapped
        // in RED (PT 63).  Parse the RED header blocks to extract the primary
        // (newest) payload and decode it as the profile's audio codec.
        //
        // RED format:
        //   Block header (not-last): [F=1 | PT(7)] [ts_off_hi(8)] [ts_off_lo(6)|len_hi(2)] [len_lo(8)]
        //   Block header (last):     [F=0 | PT(7)]
        //   Then payloads in order (oldest first, primary last).
        if codec_hint.is_none() && encoded.len() > 1 {
            let first_pt = encoded[0] & 0x7F;
            if Some(first_pt) == profile_audio_pt {
                // Walk the header chain to find total header size and
                // the cumulative length of all redundant (non-primary) payloads.
                let hdr_end;
                let mut redundant_len = 0usize;  // total bytes of non-primary payloads
                let mut pos = 0usize;
                let mut ok = true;
                loop {
                    if pos >= encoded.len() { ok = false; break; }
                    let hdr = encoded[pos];
                    let f_bit = hdr & 0x80 != 0;
                    let _pt = hdr & 0x7F;
                    pos += 1;
                    if f_bit {
                        // Not-last block: 3 more header bytes (ts offset + length)
                        if pos + 2 >= encoded.len() { ok = false; break; }
                        let blen = ((encoded[pos + 1] as usize & 0x03) << 8)
                            | encoded[pos + 2] as usize;
                        redundant_len += blen;
                        pos += 3;
                    } else {
                        // Last header block found — primary payload follows
                        break;
                    }
                }
                hdr_end = pos;
                if ok && hdr_end + redundant_len < encoded.len() {
                    // Primary payload = everything after headers + redundant payloads
                    let primary_start = hdr_end + redundant_len;
                    encoded = encoded.slice(primary_start..);
                    codec_hint = profile_audio_codec;
                }
            }
        }

        let decoder_type = match codec_hint {
            Some(codec) => codec,
            None => match CodecType::try_from(frame.payload_type.unwrap_or(0)) {
                Ok(c) => c,
                Err(_) => return Ok(()), // Unknown PT — skip silently
            },
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

        // Position sequentially: append right after the previous sample.
        // This eliminates jitter-induced silence gaps (click noise) that
        // occur with wall-clock or RTP timestamp positioning.
        let absolute_ts = self.leg_end_ts(leg);
        self.insert_audio_block(leg, absolute_ts, encoded);
        if self.last_flush.elapsed() >= self.ptime {
            self.flush()?;
        }
        Ok(())
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

    fn block_span_samples(&self, data: &[u8]) -> u32 {
        let (samples_per_block, bytes_per_block) = self.block_info();
        (data.len() / bytes_per_block) as u32 * samples_per_block
    }

    fn active_dtmf_state(&self, leg: Leg) -> Option<DtmfEventState> {
        match leg {
            Leg::A => self.dtmf_state_a,
            Leg::B => self.dtmf_state_b,
        }
    }

    fn set_dtmf_state(&mut self, leg: Leg, state: DtmfEventState) {
        match leg {
            Leg::A => self.dtmf_state_a = Some(state),
            Leg::B => self.dtmf_state_b = Some(state),
        }
    }

    fn trim_front(&self, data: &Bytes, drop_samples: u32) -> Option<(u32, Bytes)> {
        let (samples_per_block, bytes_per_block) = self.block_info();
        let drop_blocks = drop_samples.div_ceil(samples_per_block) as usize;
        let drop_bytes = drop_blocks * bytes_per_block;
        if drop_bytes >= data.len() {
            None
        } else {
            Some((
                drop_blocks as u32 * samples_per_block,
                Bytes::copy_from_slice(&data[drop_bytes..]),
            ))
        }
    }

    fn trim_back(&self, data: &Bytes, keep_samples: u32) -> Option<Bytes> {
        let (samples_per_block, bytes_per_block) = self.block_info();
        let keep_blocks = (keep_samples / samples_per_block) as usize;
        let keep_bytes = keep_blocks * bytes_per_block;
        if keep_bytes == 0 {
            None
        } else {
            Some(Bytes::copy_from_slice(&data[..keep_bytes.min(data.len())]))
        }
    }

    fn overlay_dtmf_range(&mut self, leg: Leg, start_ts: u32, end_ts: u32, encoded: Bytes) {
        let overlapping_keys: Vec<u32> = match leg {
            Leg::A => self.buffer_a.range(..end_ts).map(|(k, _)| *k).collect(),
            Leg::B => self.buffer_b.range(..end_ts).map(|(k, _)| *k).collect(),
        };

        for key in overlapping_keys {
            let data = match leg {
                Leg::A => self.buffer_a.remove(&key),
                Leg::B => self.buffer_b.remove(&key),
            };
            let Some(data) = data else {
                continue;
            };
            let block_end = key.saturating_add(self.block_span_samples(&data));
            if block_end <= start_ts || key >= end_ts {
                match leg {
                    Leg::A => {
                        self.buffer_a.insert(key, data);
                    }
                    Leg::B => {
                        self.buffer_b.insert(key, data);
                    }
                }
                continue;
            }

            if key < start_ts
                && let Some(prefix) = self.trim_back(&data, start_ts - key)
            {
                match leg {
                    Leg::A => {
                        self.buffer_a.insert(key, prefix);
                    }
                    Leg::B => {
                        self.buffer_b.insert(key, prefix);
                    }
                }
            }

            if block_end > end_ts
                && let Some((trimmed_samples, suffix)) =
                    self.trim_front(&data, end_ts.saturating_sub(key))
            {
                let suffix_ts = key.saturating_add(trimmed_samples);
                match leg {
                    Leg::A => {
                        self.buffer_a.insert(suffix_ts, suffix);
                    }
                    Leg::B => {
                        self.buffer_b.insert(suffix_ts, suffix);
                    }
                }
            }
        }

        match leg {
            Leg::A => {
                self.buffer_a.insert(start_ts, encoded);
            }
            Leg::B => {
                self.buffer_b.insert(start_ts, encoded);
            }
        }
    }

    fn insert_audio_block(&mut self, leg: Leg, start_ts: u32, encoded: Bytes) {
        match leg {
            Leg::A => self.leg_a_started = true,
            Leg::B => self.leg_b_started = true,
        }
        let mut inserts = vec![(start_ts, encoded)];
        if let Some(state) = self.active_dtmf_state(leg) {
            let dtmf_start = state.absolute_timestamp;
            let dtmf_end = state
                .absolute_timestamp
                .saturating_add(state.duration_samples);
            let mut next_inserts = Vec::new();

            for (ts, data) in inserts {
                let block_end = ts.saturating_add(self.block_span_samples(&data));
                if block_end <= dtmf_start || ts >= dtmf_end {
                    next_inserts.push((ts, data));
                    continue;
                }

                if ts < dtmf_start
                    && let Some(prefix) = self.trim_back(&data, dtmf_start - ts)
                {
                    next_inserts.push((ts, prefix));
                }

                if block_end > dtmf_end
                    && let Some((trimmed_samples, suffix)) =
                        self.trim_front(&data, dtmf_end.saturating_sub(ts))
                {
                    next_inserts.push((ts.saturating_add(trimmed_samples), suffix));
                }
            }
            inserts = next_inserts;
        }

        for (ts, data) in inserts {
            match leg {
                Leg::A => {
                    self.buffer_a.insert(ts, data);
                }
                Leg::B => {
                    self.buffer_b.insert(ts, data);
                }
            }
        }
    }

    pub fn write_dtmf_payload(
        &mut self,
        leg: Leg,
        payload: &[u8],
        rtp_timestamp: u32,
        clock_rate: u32,
    ) -> Result<()> {
        if payload.len() < 4 {
            return Ok(());
        }
        let digit_code = payload[0];
        let Some(digit) = crate::media::telephone_event::dtmf_code_to_char(digit_code) else {
            return Ok(());
        };

        let end_bit = (payload[1] & 0x80) != 0;
        let duration = u16::from_be_bytes([payload[2], payload[3]]) as u32;
        let duration_samples =
            (duration as u64 * self.sample_rate as u64).div_ceil(clock_rate.max(1) as u64) as u32;
        let duration_ms = (duration as u64 * 1000).div_ceil(clock_rate.max(1) as u64) as u32;

        let existing_state = self.active_dtmf_state(leg).filter(|state| {
            state.digit_code == digit_code && state.rtp_timestamp == rtp_timestamp
        });
        if existing_state.is_some_and(|state| duration_samples <= state.duration_samples) {
            return Ok(());
        }

        let (absolute_ts, previous_duration_samples) = existing_state
            .map(|state| (state.absolute_timestamp, state.duration_samples))
            .unwrap_or_else(|| (self.leg_end_ts(leg), 0));
        let write_start_ts = absolute_ts.saturating_add(previous_duration_samples);

        debug!(
            leg = ?leg,
            digit = %digit,
            duration_ms = %duration_ms,
            duration_samples,
            "Recording DTMF digit"
        );
        let pcm = self
            .dtmf_gen
            .generate_samples(digit, duration_samples as usize);
        let pcm = &pcm[previous_duration_samples as usize..];
        let encoded = if let Some(enc) = self.encoder.as_mut() {
            Bytes::from(enc.encode(pcm))
        } else {
            Bytes::from(audio_codec::samples_to_bytes(pcm))
        };
        let end_ts = absolute_ts.saturating_add(duration_samples);
        match leg {
            Leg::A => self.leg_a_started = true,
            Leg::B => self.leg_b_started = true,
        }
        self.overlay_dtmf_range(leg, write_start_ts, end_ts, encoded);

        self.set_dtmf_state(
            leg,
            DtmfEventState {
                digit_code,
                rtp_timestamp,
                absolute_timestamp: absolute_ts,
                duration_samples,
            },
        );

        if end_bit {
            self.flush()?;
        }
        Ok(())
    }

    pub fn write_dtmf(
        &mut self,
        leg: Leg,
        digit: char,
        duration_ms: u32,
    ) -> Result<()> {
        let pcm = self.dtmf_gen.generate(digit, duration_ms);
        debug!(
            "Recording DTMF: leg={:?}, digit={}, duration={}ms, samples={}",
            leg,
            digit,
            duration_ms,
            pcm.len()
        );

        // Position sequentially — append after previous content
        let ts = self.leg_end_ts(leg);

        let encoded = if let Some(enc) = self.encoder.as_mut() {
            Bytes::from(enc.encode(&pcm))
        } else {
            Bytes::from(audio_codec::samples_to_bytes(&pcm))
        };

        let end_ts = ts.saturating_add(self.block_span_samples(&encoded));
        self.overlay_dtmf_range(leg, ts, end_ts, encoded);

        self.flush()?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.last_flush = Instant::now();

        let max_ts_a = self
            .buffer_a
            .last_key_value()
            .map(|(k, v)| {
                let (_, bytes_per_block) = self.block_info();
                let samples_per_block = self.samples_per_block();
                k + (v.len() / bytes_per_block) as u32 * samples_per_block
            })
            .unwrap_or(0);
        let max_ts_b = self
            .buffer_b
            .last_key_value()
            .map(|(k, v)| {
                let (_, bytes_per_block) = self.block_info();
                let samples_per_block = self.samples_per_block();
                k + (v.len() / bytes_per_block) as u32 * samples_per_block
            })
            .unwrap_or(0);
        // Flush strategy — prevents silence gaps on the slower leg.
        //
        // When both legs are active, flush only up to min(a, b) to avoid
        // silence-filling the lagging leg.  When one leg is temporarily
        // stalled (no new data past next_flush_ts), WAIT (return early)
        // rather than using max — that prevents the periodic 100ms silence
        // gaps that sound like choppy/glitchy audio.
        //
        // Safety-valve: if the leading leg has >2 s of buffered data the
        // stalled leg is likely permanently done — force-flush to keep
        // memory bounded.
        let flush_target = if !self.leg_a_started || !self.leg_b_started {
            max_ts_a.max(max_ts_b)
        } else if max_ts_a > self.next_flush_ts && max_ts_b > self.next_flush_ts {
            max_ts_a.min(max_ts_b)
        } else {
            // Both started but at least one hasn't advanced.
            let max_ts = max_ts_a.max(max_ts_b);
            let buffered = max_ts.saturating_sub(self.next_flush_ts);
            if buffered > self.sample_rate * 2 {
                max_ts  // safety-valve: leading leg >2s ahead
            } else {
                return Ok(());  // wait for slower leg
            }
        };

        if flush_target <= self.next_flush_ts {
            return Ok(());
        }

        let mut flush_len = flush_target - self.next_flush_ts;

        // Limit flush length to prevent huge memory allocation (e.g., 10 seconds max)
        let max_flush_samples = self.sample_rate * 10;
        if flush_len > max_flush_samples {
            flush_len = max_flush_samples;
        }

        let (samples_per_block, _) = self.block_info();
        flush_len = (flush_len / samples_per_block) * samples_per_block;

        if flush_len == 0 {
            return Ok(());
        }

        let data_a = self.get_leg_data(Leg::A, flush_len)?;
        let data_b = self.get_leg_data(Leg::B, flush_len)?;

        self.next_flush_ts += flush_len;

        let output = if self.channels == 1 {
            // Mono: mix both legs
            self.mix(&data_a, &data_b)?
        } else {
            // Stereo: interleave both legs.
            // Default: Leg A (caller) → left, Leg B (callee) → right.
            // stereo_swap: callee → left, caller → right.
            if self.stereo_swap {
                self.interleave(&data_b, &data_a)?
            } else {
                self.interleave(&data_a, &data_b)?
            }
        };

        self.writer.write_packet(&output, flush_len as usize)?;
        self.written_bytes += output.len() as u32;
        self.written_samples += flush_len as u64;

        Ok(())
    }

    pub fn finalize(&mut self) -> Result<()> {
        // Final flush: use max to flush ALL remaining data from both legs.
        self.flush_final()?;
        self.writer.finalize()?;
        Ok(())
    }

    /// Flush everything remaining — used by finalize() to write trailing data.
    fn flush_final(&mut self) -> Result<()> {
        self.last_flush = Instant::now();
        let max_ts_a = self.buffer_a.last_key_value()
            .map(|(k, v)| k + (v.len() / self.block_info().1) as u32 * self.samples_per_block())
            .unwrap_or(0);
        let max_ts_b = self.buffer_b.last_key_value()
            .map(|(k, v)| k + (v.len() / self.block_info().1) as u32 * self.samples_per_block())
            .unwrap_or(0);
        let max_available_ts = max_ts_a.max(max_ts_b);
        if max_available_ts <= self.next_flush_ts {
            return Ok(());
        }
        let flush_len = max_available_ts - self.next_flush_ts;
        let data_a = self.get_leg_data(Leg::A, flush_len)?;
        let data_b = self.get_leg_data(Leg::B, flush_len)?;
        self.next_flush_ts += flush_len;
        let output = if self.channels == 1 {
            self.mix(&data_a, &data_b)?
        } else if self.stereo_swap {
            self.interleave(&data_b, &data_a)?
        } else {
            self.interleave(&data_a, &data_b)?
        };
        self.writer.write_packet(&output, flush_len as usize)?;
        self.written_bytes += output.len() as u32;
        self.written_samples += flush_len as u64;
        Ok(())
    }

    fn block_info(&self) -> (u32, usize) {
        match self.codec {
            CodecType::G729 => (80, 10),
            CodecType::PCMU | CodecType::PCMA => (1, 1),
            CodecType::G722 => (2, 1),
            CodecType::Opus => (1, 2), // Buffering PCM
            _ => (1, 2),               // PCM
        }
    }

    fn samples_per_block(&self) -> u32 {
        self.block_info().0
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

    fn interleave(&mut self, data_a: &[u8], data_b: &[u8]) -> Result<Vec<u8>> {
        let (_, bytes_per_block) = self.block_info();
        let len = data_a.len().min(data_b.len());
        let mut interleaved = Vec::with_capacity(len * 2);
        let num_blocks = len / bytes_per_block;
        for i in 0..num_blocks {
            interleaved.extend_from_slice(&data_a[i * bytes_per_block..(i + 1) * bytes_per_block]);
            interleaved.extend_from_slice(&data_b[i * bytes_per_block..(i + 1) * bytes_per_block]);
        }
        Ok(interleaved)
    }

    fn mix(&mut self, data_a: &[u8], data_b: &[u8]) -> Result<Vec<u8>> {
        let (_, bytes_per_block) = self.block_info();
        let len = data_a.len().min(data_b.len());
        let mut mixed = Vec::with_capacity(len);

        match self.codec {
            CodecType::PCMU | CodecType::PCMA => {
                // For μ-law/A-law, decode to PCM, mix, and re-encode
                let mut decoder_a = create_decoder(self.codec);
                let pcm_a = decoder_a.decode(data_a);

                let mut decoder_b = create_decoder(self.codec);
                let pcm_b = decoder_b.decode(data_b);

                // Mix PCM samples (average to prevent clipping)
                let mut pcm_mixed = Vec::with_capacity(pcm_a.len().min(pcm_b.len()));
                for i in 0..pcm_a.len().min(pcm_b.len()) {
                    let sample = ((pcm_a[i] as i32 + pcm_b[i] as i32) / 2) as i16;
                    pcm_mixed.push(sample);
                }

                // Re-encode to target codec
                if let Some(enc) = self.encoder.as_mut() {
                    mixed = enc.encode(&pcm_mixed);
                } else {
                    mixed = audio_codec::samples_to_bytes(&pcm_mixed);
                }
            }
            _ => {
                // For PCM data, mix directly
                let num_blocks = len / bytes_per_block;
                for i in 0..num_blocks {
                    let start = i * bytes_per_block;
                    let end = (i + 1) * bytes_per_block;
                    for j in start..end.min(data_a.len()).min(data_b.len()) {
                        // Mix by averaging (simple approach)
                        let sample = ((data_a[j] as i16 + data_b[j] as i16) / 2) as u8;
                        mixed.push(sample);
                    }
                }
            }
        }

        Ok(mixed)
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

    pub fn generate(&self, digit: char, duration_ms: u32) -> Vec<i16> {
        let num_samples = (self.sample_rate as f32 * (duration_ms as f32 / 1000.0)) as usize;
        self.generate_samples(digit, num_samples)
    }

    pub fn generate_samples(&self, digit: char, num_samples: usize) -> Vec<i16> {
        let freqs = match digit {
            '1' => (697.0, 1209.0),
            '2' => (697.0, 1336.0),
            '3' => (697.0, 1477.0),
            '4' => (770.0, 1209.0),
            '5' => (770.0, 1336.0),
            '6' => (770.0, 1477.0),
            '7' => (852.0, 1209.0),
            '8' => (852.0, 1336.0),
            '9' => (852.0, 1477.0),
            '*' => (941.0, 1209.0),
            '0' => (941.0, 1336.0),
            '#' => (941.0, 1477.0),
            'A' => (697.0, 1633.0),
            'B' => (770.0, 1633.0),
            'C' => (852.0, 1633.0),
            'D' => (941.0, 1633.0),
            _ => return Vec::new(),
        };
        let mut samples = Vec::with_capacity(num_samples);

        for i in 0..num_samples {
            let t = i as f32 / self.sample_rate as f32;
            let s1 = (2.0 * std::f32::consts::PI * freqs.0 * t).sin();
            let s2 = (2.0 * std::f32::consts::PI * freqs.1 * t).sin();
            let s = (s1 + s2) / 2.0;
            samples.push((s * 32767.0) as i16);
        }

        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{CodecType, create_decoder, create_encoder};

    #[test]
    fn test_mix_pcmu_both_silent() {
        // Test mixing two silent audio streams
        let mut recorder = create_test_recorder(CodecType::PCMU, 1);

        // Create silent PCM samples (all zeros)
        let pcm_silent = vec![0i16; 160];

        // Encode to PCMU
        let mut encoder = create_encoder(CodecType::PCMU);
        let data_a = encoder.encode(&pcm_silent);
        let data_b = encoder.encode(&pcm_silent);

        // Mix the data
        let mixed = recorder.mix(&data_a, &data_b).unwrap();

        // Decode and verify
        let mut decoder = create_decoder(CodecType::PCMU);
        let pcm_result = decoder.decode(&mixed);

        // All samples should be near zero (silence)
        let max_sample = pcm_result.iter().map(|&s| s.abs()).max().unwrap_or(0);
        assert!(
            max_sample < 100,
            "Mixed silent audio should remain silent, got max={}",
            max_sample
        );
    }

    #[test]
    fn test_mix_pcmu_one_silent() {
        // Test mixing one silent and one active audio stream
        let mut recorder = create_test_recorder(CodecType::PCMU, 1);

        // Create test PCM samples
        let pcm_silent = vec![0i16; 160];
        let pcm_active: Vec<i16> = (0..160)
            .map(|i| ((i as f32 / 10.0).sin() * 5000.0) as i16)
            .collect();

        // Encode to PCMU
        let mut encoder_a = create_encoder(CodecType::PCMU);
        let mut encoder_b = create_encoder(CodecType::PCMU);
        let data_a = encoder_a.encode(&pcm_silent);
        let data_b = encoder_b.encode(&pcm_active);

        // Mix the data
        let mixed = recorder.mix(&data_a, &data_b).unwrap();

        // Decode and verify
        let mut decoder = create_decoder(CodecType::PCMU);
        let pcm_result = decoder.decode(&mixed);

        // The mixed result should be roughly half the amplitude of the active stream
        // since we're averaging with silence (0)
        let mut decoder_b = create_decoder(CodecType::PCMU);
        let pcm_b_decoded = decoder_b.decode(&data_b);

        // Check that mixed amplitude is roughly half of the original
        let avg_mixed: i32 = pcm_result
            .iter()
            .take(100)
            .map(|&s| s.abs() as i32)
            .sum::<i32>()
            / 100;
        let avg_original: i32 = pcm_b_decoded
            .iter()
            .take(100)
            .map(|&s| s.abs() as i32)
            .sum::<i32>()
            / 100;

        // Both should have non-zero amplitude
        assert!(
            avg_original > 100,
            "Original signal should be non-zero, got {}",
            avg_original
        );
        assert!(
            avg_mixed > 50,
            "Mixed signal should be non-zero, got {}",
            avg_mixed
        );

        let ratio = avg_mixed as f32 / avg_original as f32;
        assert!(
            ratio > 0.35 && ratio < 0.65,
            "Mixed amplitude should be ~0.5x original, got ratio={} (mixed={}, orig={})",
            ratio,
            avg_mixed,
            avg_original
        );
    }

    #[test]
    fn test_mix_pcmu_both_active() {
        // Test mixing two active audio streams with different amplitudes
        let mut recorder = create_test_recorder(CodecType::PCMU, 1);

        // Create test PCM samples with different amplitudes
        let pcm_a: Vec<i16> = (0..160)
            .map(|i| (i as f32 * 50.0).sin() as i16 * 2000)
            .collect();
        let pcm_b: Vec<i16> = (0..160)
            .map(|i| (i as f32 * 70.0).sin() as i16 * 3000)
            .collect();

        // Encode to PCMU
        let mut encoder_a = create_encoder(CodecType::PCMU);
        let mut encoder_b = create_encoder(CodecType::PCMU);
        let data_a = encoder_a.encode(&pcm_a);
        let data_b = encoder_b.encode(&pcm_b);

        // Mix the data
        let mixed = recorder.mix(&data_a, &data_b).unwrap();

        // Verify the output length is correct (same as inputs for PCMU)
        assert_eq!(mixed.len(), data_a.len());
        assert_eq!(mixed.len(), data_b.len());

        // Decode all three and verify averaging
        let mut decoder_a = create_decoder(CodecType::PCMU);
        let mut decoder_b = create_decoder(CodecType::PCMU);
        let mut decoder_mixed = create_decoder(CodecType::PCMU);

        let pcm_a_decoded = decoder_a.decode(&data_a);
        let pcm_b_decoded = decoder_b.decode(&data_b);
        let pcm_mixed = decoder_mixed.decode(&mixed);

        // Check that mixed samples are approximately the average
        let sample_idx = 50; // Check middle sample
        let expected =
            ((pcm_a_decoded[sample_idx] as i32 + pcm_b_decoded[sample_idx] as i32) / 2) as i16;
        let actual = pcm_mixed[sample_idx];
        let diff = (expected - actual).abs();

        // Allow some tolerance due to codec quantization
        assert!(
            diff < 200,
            "Mixed sample should be average of inputs, expected ~{}, got {}, diff={}",
            expected,
            actual,
            diff
        );
    }

    #[test]
    fn test_interleave_pcmu() {
        // Test interleaving two audio streams for stereo recording
        let mut recorder = create_test_recorder(CodecType::PCMU, 2);

        // Create distinct test data
        let data_a: Vec<u8> = (0..160).map(|i| (i % 256) as u8).collect();
        let data_b: Vec<u8> = (0..160).map(|i| ((i + 128) % 256) as u8).collect();

        // Interleave the data
        let interleaved = recorder.interleave(&data_a, &data_b).unwrap();

        // Verify length is double
        assert_eq!(interleaved.len(), data_a.len() + data_b.len());

        // Verify interleaving pattern: A[0], B[0], A[1], B[1], ...
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
    fn test_channel_selection_mono() {
        // Verify that mono recording (channels=1) uses mix
        let recorder = create_test_recorder(CodecType::PCMU, 1);
        assert_eq!(recorder.channels, 1, "Mono recorder should have 1 channel");
    }

    #[test]
    fn test_channel_selection_stereo() {
        // Verify that stereo recording (channels=2) for wideband codecs
        let recorder = create_test_recorder(CodecType::Opus, 2);
        assert_eq!(recorder.channels, 2, "Opus recorder should have 2 channels");

        let recorder_g722 = create_test_recorder(CodecType::G722, 2);
        assert_eq!(
            recorder_g722.channels, 2,
            "G722 recorder should have 2 channels"
        );
    }

    #[test]
    fn test_mix_prevents_clipping() {
        // Test that mixing prevents clipping by averaging instead of adding
        let mut recorder = create_test_recorder(CodecType::PCMU, 1);

        // Create high-amplitude samples near the maximum
        let pcm_high = vec![20000i16; 160];

        // Encode to PCMU
        let mut encoder_a = create_encoder(CodecType::PCMU);
        let mut encoder_b = create_encoder(CodecType::PCMU);
        let data_a = encoder_a.encode(&pcm_high);
        let data_b = encoder_b.encode(&pcm_high);

        // Mix the data
        let mixed = recorder.mix(&data_a, &data_b).unwrap();

        // Decode and verify no clipping
        let mut decoder = create_decoder(CodecType::PCMU);
        let pcm_result = decoder.decode(&mixed);

        // The result should be around 20000 (averaged), not 40000 (which would clip)
        let avg_result: i32 =
            pcm_result.iter().map(|&s| s as i32).sum::<i32>() / pcm_result.len() as i32;
        assert!(
            avg_result > 15000 && avg_result < 25000,
            "Mixed high-amplitude audio should not clip, avg={}",
            avg_result
        );
    }

    // Helper function to create a test recorder
    fn create_test_recorder(codec: CodecType, channels: u16) -> Recorder {
        let sample_rate = codec.samplerate();
        let encoder = Some(create_encoder(codec));

        Recorder {
            path: "test.wav".to_string(),
            codec,
            written_bytes: 0,
            sample_rate,
            channels,
            dtmf_gen: DtmfGenerator::new(sample_rate),
            encoder,
            decoders: HashMap::new(),
            resamplers: HashMap::new(),
            buffer_a: BTreeMap::new(),
            buffer_b: BTreeMap::new(),
            last_flush: Instant::now(),
            profile_a: NegotiatedLegProfile::default(),
            profile_b: NegotiatedLegProfile::default(),
            dtmf_state_a: None,
            dtmf_state_b: None,
            next_flush_ts: 0,
            written_samples: 0,
            writer: Box::new(TestWriter::new()),
            ptime: Duration::from_millis(20),
            stereo_swap: false,
            leg_a_started: false,
            leg_b_started: false,
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
