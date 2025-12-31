use anyhow::Result;
use audio_codec::{CodecType, Decoder, Encoder, create_decoder, create_encoder};
use rustrtc::media::{MediaSample, MediaStreamTrack};
use rustrtc::{PeerConnection, PeerConnectionEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::debug;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecorderFormat {
    Wav,
    G729,
    Ogg,
}

impl RecorderFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            RecorderFormat::Wav => "wav",
            RecorderFormat::Ogg => "ogg",
            RecorderFormat::G729 => "g729",
        }
    }

    pub fn is_supported(&self) -> bool {
        match self {
            RecorderFormat::Wav | RecorderFormat::G729 => true,
            RecorderFormat::Ogg => cfg!(feature = "opus"),
        }
    }

    pub fn effective(&self) -> RecorderFormat {
        if self.is_supported() {
            *self
        } else {
            RecorderFormat::Wav
        }
    }
}

impl Default for RecorderFormat {
    fn default() -> Self {
        RecorderFormat::Wav
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<RecorderFormat>,
}

impl RecorderOption {
    pub fn new(recorder_file: String) -> Self {
        Self {
            recorder_file,
            ..Default::default()
        }
    }

    pub fn resolved_format(&self, default: RecorderFormat) -> RecorderFormat {
        self.format.unwrap_or(default).effective()
    }

    pub fn ensure_path_extension(&mut self, fallback_format: RecorderFormat) {
        let effective_format = self.format.unwrap_or(fallback_format).effective();
        self.format = Some(effective_format);

        if self.recorder_file.is_empty() {
            return;
        }

        let mut path = PathBuf::from(&self.recorder_file);
        let has_desired_ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case(effective_format.extension()))
            .unwrap_or(false);

        if !has_desired_ext {
            path.set_extension(effective_format.extension());
            self.recorder_file = path.to_string_lossy().into_owned();
        }
    }
}

impl Default for RecorderOption {
    fn default() -> Self {
        Self {
            recorder_file: "".to_string(),
            samplerate: 16000,
            ptime: 200,
            format: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Leg {
    A,
    B,
}

struct RawPacket {
    timestamp: u32,
    data: Vec<u8>,
}

pub struct Recorder {
    file: File,
    codec: CodecType,
    written_bytes: u32,
    sample_rate: u32,
    channels: u16,
    dtmf_gen: DtmfGenerator,
    encoder: Box<dyn Encoder>,
    decoder_a: Box<dyn Decoder>,
    decoder_b: Box<dyn Decoder>,
    buffer_a: BTreeMap<u32, RawPacket>,
    buffer_b: BTreeMap<u32, RawPacket>,
    last_flush: Instant,
    base_timestamp_a: Option<u32>,
    base_timestamp_b: Option<u32>,
    written_samples: u64,
}

impl Recorder {
    pub fn new(
        path: &str,
        codec: CodecType,
        codec_a: CodecType,
        codec_b: CodecType,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Self> {
        // ensure the directory exists
        if let Some(parent) = PathBuf::from(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut file = File::create(path)
            .map_err(|e| anyhow::anyhow!("Failed to create recorder file {}: {}", path, e))?;

        // If WAV, write placeholder header
        if Self::is_wav_format(codec) {
            Self::write_wav_header(&mut file, codec, sample_rate, channels, 0)?;
        }

        let encoder = create_encoder(codec);
        let decoder_a = create_decoder(codec_a);
        let decoder_b = create_decoder(codec_b);

        Ok(Self {
            file,
            codec,
            written_bytes: 0,
            sample_rate,
            channels,
            dtmf_gen: DtmfGenerator::new(sample_rate),
            encoder,
            decoder_a,
            decoder_b,
            buffer_a: BTreeMap::new(),
            buffer_b: BTreeMap::new(),
            last_flush: Instant::now(),
            base_timestamp_a: None,
            base_timestamp_b: None,
            written_samples: 0,
        })
    }

    pub fn is_wav_format(codec: CodecType) -> bool {
        match codec {
            CodecType::PCMU | CodecType::PCMA | CodecType::G722 => true,
            _ => false,
        }
    }

    pub fn write_sample(
        &mut self,
        leg: Leg,
        sample: &MediaSample,
        dtmf_pt: Option<u8>,
    ) -> Result<()> {
        match sample {
            MediaSample::Audio(frame) => {
                if let (Some(pt), Some(dpt)) = (frame.payload_type, dtmf_pt) {
                    if pt == dpt {
                        return self.write_dtmf_payload(leg, &frame.data);
                    }
                }

                let packet = RawPacket {
                    timestamp: frame.rtp_timestamp,
                    data: frame.data.to_vec(),
                };

                match leg {
                    Leg::A => {
                        if self.base_timestamp_a.is_none() {
                            self.base_timestamp_a = Some(packet.timestamp);
                            debug!("Recorder Leg A: base_timestamp={} seq={:?}", packet.timestamp, frame.sequence_number);
                        }
                        let buffer_size = self.buffer_a.len();
                        self.buffer_a.insert(packet.timestamp, packet);
                        // Log if buffer is growing (possible jitter or out-of-order packets)
                        if buffer_size > 10 {
                            debug!("Recorder Leg A: buffer size growing, now {} packets", buffer_size);
                        }
                    }
                    Leg::B => {
                        if self.base_timestamp_b.is_none() {
                            self.base_timestamp_b = Some(packet.timestamp);
                            debug!("Recorder Leg B: base_timestamp={} seq={:?}", packet.timestamp, frame.sequence_number);
                        }
                        let buffer_size = self.buffer_b.len();
                        self.buffer_b.insert(packet.timestamp, packet);
                        // Log if buffer is growing (possible jitter or out-of-order packets)
                        if buffer_size > 10 {
                            debug!("Recorder Leg B: buffer size growing, now {} packets", buffer_size);
                        }
                    }
                }
            }
            _ => {}
        }

        if self.last_flush.elapsed() >= Duration::from_millis(200) {
            self.flush()?;
        }
        Ok(())
    }

    pub fn write_dtmf_payload(&mut self, leg: Leg, payload: &[u8]) -> Result<()> {
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
            self.write_dtmf(leg, digit, duration_ms)
        } else {
            Ok(())
        }
    }

    pub fn write_dtmf(&mut self, leg: Leg, digit: char, duration_ms: u32) -> Result<()> {
        let pcm = self.dtmf_gen.generate(digit, duration_ms);
        // For now, we just encode and write it directly to the file,
        // but in 2-channel mode we should ideally align it.
        // To keep it simple, we'll just flush first, then write DTMF.
        self.flush()?;

        let mut interleaved = Vec::with_capacity(pcm.len() * self.channels as usize);
        for sample in pcm {
            if self.channels == 2 {
                if leg == Leg::A {
                    interleaved.push(sample);
                    interleaved.push(0);
                } else {
                    interleaved.push(0);
                    interleaved.push(sample);
                }
            } else {
                interleaved.push(sample);
            }
        }

        let encoded = self.encoder.encode(&interleaved);
        self.file.write_all(&encoded)?;
        self.written_bytes += encoded.len() as u32;
        self.written_samples += (interleaved.len() / self.channels as usize) as u64;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.last_flush = Instant::now();

        let packet_count_a = self.buffer_a.len();
        let packet_count_b = self.buffer_b.len();

        let mut pcm_a = Vec::new();
        while let Some((_, packet)) = self.buffer_a.pop_first() {
            let mut decoded = self.decoder_a.decode(&packet.data);
            pcm_a.append(&mut decoded);
        }

        let mut pcm_b = Vec::new();
        while let Some((_, packet)) = self.buffer_b.pop_first() {
            let mut decoded = self.decoder_b.decode(&packet.data);
            pcm_b.append(&mut decoded);
        }

        if pcm_a.is_empty() && pcm_b.is_empty() {
            return Ok(());
        }

        // Log flush statistics every 10 flushes (every ~2 seconds)
        if self.written_samples % 16000 < 8000 {
            debug!(
                "Recorder flush: packets_a={} packets_b={} samples_a={} samples_b={} total_written={}",
                packet_count_a, packet_count_b, pcm_a.len(), pcm_b.len(), self.written_samples
            );
        }

        let len = pcm_a.len().max(pcm_b.len());
        let mut interleaved = Vec::with_capacity(len * self.channels as usize);

        for i in 0..len {
            if self.channels == 2 {
                interleaved.push(if i < pcm_a.len() { pcm_a[i] } else { 0 });
                interleaved.push(if i < pcm_b.len() { pcm_b[i] } else { 0 });
            } else {
                // Mix or just take A
                let val = (if i < pcm_a.len() { pcm_a[i] as i32 } else { 0 })
                    + (if i < pcm_b.len() { pcm_b[i] as i32 } else { 0 });
                interleaved.push((val.clamp(-32768, 32767)) as i16);
            }
        }

        let encoded = self.encoder.encode(&interleaved);
        self.file.write_all(&encoded)?;
        self.written_bytes += encoded.len() as u32;
        self.written_samples += len as u64;

        Ok(())
    }

    pub fn finalize(&mut self) -> Result<()> {
        self.flush()?;
        if Self::is_wav_format(self.codec) {
            // Update WAV header with actual size
            self.file.seek(SeekFrom::Start(0))?;
            Self::write_wav_header(
                &mut self.file,
                self.codec,
                self.sample_rate,
                self.channels,
                self.written_bytes,
            )?;
        }
        Ok(())
    }

    fn write_wav_header(
        file: &mut File,
        codec: CodecType,
        sample_rate: u32,
        channels: u16,
        data_size: u32,
    ) -> Result<()> {
        let mut header = [0u8; 44];
        header[0..4].copy_from_slice(b"RIFF");
        let file_size = 36 + data_size;
        header[4..8].copy_from_slice(&file_size.to_le_bytes());
        header[8..12].copy_from_slice(b"WAVE");
        header[12..16].copy_from_slice(b"fmt ");
        header[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size

        let format_tag: u16 = match codec {
            CodecType::PCMU => 7,      // mu-law
            CodecType::PCMA => 6,      // a-law
            CodecType::G722 => 0x028F, // G.722 (sometimes 1 or others, but 0x028F is common for G.722)
            _ => 1,                    // PCM
        };

        header[20..22].copy_from_slice(&format_tag.to_le_bytes());
        header[22..24].copy_from_slice(&channels.to_le_bytes());
        header[24..28].copy_from_slice(&sample_rate.to_le_bytes());

        let bits_per_sample: u16 = match codec {
            CodecType::PCMU | CodecType::PCMA => 8,
            CodecType::G722 => 8, // G.722 is 8 bits per sample (compressed)
            _ => 16,
        };

        let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
        let block_align = channels * (bits_per_sample / 8);

        header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        header[32..34].copy_from_slice(&block_align.to_le_bytes());
        header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
        header[36..40].copy_from_slice(b"data");
        header[40..44].copy_from_slice(&data_size.to_le_bytes());

        file.write_all(&header)?;
        Ok(())
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

pub async fn record_pc_tracks(
    pc: PeerConnection,
    option: RecorderOption,
    _track_id: String,
    cancel_token: CancellationToken,
) -> Result<()> {
    // For single PC recording, we use PCMU as default recording codec if not specified
    let record_codec = CodecType::PCMU;
    let sample_rate = 8000;
    let channels = 1;

    let recorder = Arc::new(Mutex::new(Recorder::new(
        &option.recorder_file,
        record_codec,
        record_codec, // codec_a
        record_codec, // codec_b
        sample_rate,
        channels,
    )?));

    let pc_receiver = pc.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                debug!("PC recording cancelled");
            }
            _ = async {
                while let Some(event) = pc_receiver.recv().await {
                    if let PeerConnectionEvent::Track(transceiver) = event {
                        if let Some(receiver) = transceiver.receiver() {
                            let track = receiver.track();
                            let recorder_clone = recorder.clone();
                            let cancel_token_clone = cancel_token.clone();

                            tokio::spawn(async move {
                                tokio::select! {
                                    _ = cancel_token_clone.cancelled() => {}
                                    _ = async {
                                        while let Ok(sample) = track.recv().await {
                                            let mut guard = recorder_clone.lock().unwrap();
                                            let _ = guard.write_sample(Leg::A, &sample, None);
                                        }
                                    } => {}
                                }
                            });
                        }
                    }
                }
            } => {}
        }
    });

    Ok(())
}
