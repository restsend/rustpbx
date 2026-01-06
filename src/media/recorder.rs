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

pub struct Recorder {
    pub path: String,
    pub codec: CodecType,
    written_bytes: u32,
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
    last_flush: Instant,

    base_timestamp_a: Option<u32>,
    base_timestamp_b: Option<u32>,
    start_offset_a: u32, // in samples
    start_offset_b: u32, // in samples

    next_flush_ts: u32, // The next timestamp to be flushed
    ptime: Duration,

    written_samples: u64,
    writer: Box<dyn StreamWriter>,
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
            _ => codec,
        };

        let sample_rate = codec.samplerate();
        let encoder = Some(create_encoder(codec));
        debug!(
            "Creating recorder: path={}, src_codec={:?} codec={:?}",
            path, src_codec, codec
        );
        let mut writer = Box::new(WavWriter::new(file, sample_rate, 2, Some(codec)));
        writer.write_header()?;

        Ok(Self {
            path: path.to_string(),
            codec,
            written_bytes: 0,
            sample_rate,
            dtmf_gen: DtmfGenerator::new(sample_rate),
            encoder,
            decoders: HashMap::new(),
            resamplers: HashMap::new(),
            buffer_a: BTreeMap::new(),
            buffer_b: BTreeMap::new(),
            start_instant: Instant::now(),
            last_flush: Instant::now(),
            base_timestamp_a: None,
            base_timestamp_b: None,
            start_offset_a: 0,
            start_offset_b: 0,
            next_flush_ts: 0,
            written_samples: 0,
            writer,
            ptime: Duration::from_millis(200),
        })
    }

    pub fn write_sample(
        &mut self,
        leg: Leg,
        sample: &MediaSample,
        dtmf_pt: Option<u8>,
    ) -> Result<()> {
        let frame = match sample {
            MediaSample::Audio(frame) => frame,
            _ => return Ok(()),
        };

        if let (Some(pt), Some(dpt)) = (frame.payload_type, dtmf_pt) {
            if pt == dpt {
                return self.write_dtmf_payload(leg, &frame.data, frame.rtp_timestamp);
            }
        }

        let mut encoded = match sample {
            MediaSample::Audio(frame) => frame.data.clone(),
            _ => return Ok(()),
        };
        let decoder_type = CodecType::try_from(frame.payload_type.unwrap_or(0))?;
        let mut decoder_samplerate = self.sample_rate;

        if decoder_type != self.codec {
            let decoder = self
                .decoders
                .entry((leg, decoder_type.payload_type()))
                .or_insert_with(|| create_decoder(decoder_type));
            let pcm = decoder.decode(&encoded);
            decoder_samplerate = decoder.sample_rate();
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
                    (relative as u64 * self.sample_rate as u64 / decoder_samplerate as u64) as u32;
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
                    (relative as u64 * self.sample_rate as u64 / decoder_samplerate as u64) as u32;
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
        if self.last_flush.elapsed() >= self.ptime {
            self.flush()?;
        }
        Ok(())
    }

    pub fn write_dtmf_payload(&mut self, leg: Leg, payload: &[u8], timestamp: u32) -> Result<()> {
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
        if end_bit {
            let duration = u16::from_be_bytes([payload[2], payload[3]]);
            let duration_ms = (duration as u32 * 1000) / self.sample_rate;
            debug!(leg = ?leg, digit = %digit, duration_ms = %duration_ms, "Recording DTMF digit");
            self.write_dtmf(leg, digit, duration_ms, Some(timestamp))
        } else {
            Ok(())
        }
    }

    pub fn write_dtmf(
        &mut self,
        leg: Leg,
        digit: char,
        duration_ms: u32,
        timestamp: Option<u32>,
    ) -> Result<()> {
        let pcm = self.dtmf_gen.generate(digit, duration_ms);
        debug!(
            "Recording DTMF: leg={:?}, digit={}, duration={}ms, samples={}",
            leg,
            digit,
            duration_ms,
            pcm.len()
        );

        // Determine absolute timestamp
        let ts = if let Some(t) = timestamp {
            match leg {
                Leg::A => {
                    if self.base_timestamp_a.is_none() {
                        self.base_timestamp_a = Some(t);
                        self.start_offset_a = (self.start_instant.elapsed().as_millis() as u64
                            * self.sample_rate as u64
                            / 1000) as u32;
                    }
                    let relative = t.wrapping_sub(self.base_timestamp_a.unwrap());
                    self.start_offset_a.wrapping_add(relative)
                }
                Leg::B => {
                    if self.base_timestamp_b.is_none() {
                        self.base_timestamp_b = Some(t);
                        self.start_offset_b = (self.start_instant.elapsed().as_millis() as u64
                            * self.sample_rate as u64
                            / 1000) as u32;
                    }
                    let relative = t.wrapping_sub(self.base_timestamp_b.unwrap());
                    self.start_offset_b.wrapping_add(relative)
                }
            }
        } else {
            // If no timestamp provided, append to the end of the buffer
            match leg {
                Leg::A => self
                    .buffer_a
                    .last_key_value()
                    .map(|(k, v)| k + v.len() as u32)
                    .unwrap_or(self.next_flush_ts),
                Leg::B => self
                    .buffer_b
                    .last_key_value()
                    .map(|(k, v)| k + v.len() as u32)
                    .unwrap_or(self.next_flush_ts),
            }
        };

        let encoded = if let Some(enc) = self.encoder.as_mut() {
            enc.encode(&pcm).into()
        } else {
            audio_codec::samples_to_bytes(&pcm).into()
        };

        match leg {
            Leg::A => {
                self.buffer_a.insert(ts, encoded);
            }
            Leg::B => {
                self.buffer_b.insert(ts, encoded);
            }
        }

        self.flush()?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
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
        let max_available_ts = max_ts_a.max(max_ts_b);

        if max_available_ts <= self.next_flush_ts {
            return Ok(());
        }

        let mut flush_len = max_available_ts - self.next_flush_ts;
        let (samples_per_block, _) = self.block_info();
        flush_len = (flush_len / samples_per_block) * samples_per_block;

        if flush_len == 0 {
            return Ok(());
        }

        let data_a = self.get_leg_data(Leg::A, flush_len)?;
        let data_b = self.get_leg_data(Leg::B, flush_len)?;

        self.next_flush_ts += flush_len;

        let interleaved = self.interleave(&data_a, &data_b)?;
        self.writer.write_packet(&interleaved, flush_len as usize)?;
        self.written_bytes += interleaved.len() as u32;
        self.written_samples += flush_len as u64;

        Ok(())
    }

    pub fn finalize(&mut self) -> Result<()> {
        self.flush()?;
        self.writer.finalize()?;
        Ok(())
    }

    fn block_info(&self) -> (u32, usize) {
        match self.codec {
            CodecType::G729 => (80, 10),
            CodecType::PCMU | CodecType::PCMA => (1, 1),
            CodecType::G722 => (1, 1),
            CodecType::Opus => (1, 2), // Buffering PCM
            _ => (1, 2),               // PCM
        }
    }

    fn samples_per_block(&self) -> u32 {
        self.block_info().0
    }

    fn get_silence_bytes(&mut self, _leg: Leg, blocks: usize) -> Result<Bytes> {
        let (samples_per_block, _) = self.block_info();
        let silence_pcm = vec![0i16; (blocks as u32 * samples_per_block) as usize];

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
                        let gap_samples = ts - current_ts;
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

        let num_samples = (self.sample_rate as f32 * (duration_ms as f32 / 1000.0)) as usize;
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
