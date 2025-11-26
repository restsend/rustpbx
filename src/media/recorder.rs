use crate::media::{AudioFrame, PcmBuf, Samples, codecs::samples_to_bytes};
use anyhow::{Result, anyhow};
use futures::StreamExt;
use hound::{SampleFormat, WavSpec};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
    u32,
};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt},
    select,
    sync::mpsc::UnboundedReceiver,
};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[cfg(feature = "opus")]
use opusic_sys::{
    OPUS_APPLICATION_AUDIO, OPUS_OK, OpusEncoder as OpusEncoderRaw, opus_encode,
    opus_encoder_create, opus_encoder_destroy, opus_strerror,
};
#[cfg(feature = "opus")]
use std::{ffi::CStr, os::raw::c_int, ptr::NonNull};

#[cfg(feature = "opus")]
fn opus_error_message(code: c_int) -> String {
    if code == OPUS_OK {
        return "ok".to_string();
    }

    unsafe {
        let ptr = opus_strerror(code);
        if ptr.is_null() {
            format!("error code {code}")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecorderFormat {
    Wav,
    Ogg,
}

#[cfg(feature = "opus")]
struct OggStreamWriter {
    encoder: NonNull<OpusEncoderRaw>,
    serial: u32,
    sequence: u32,
    granule_position: u64,
    sample_rate: u32,
}

#[cfg(feature = "opus")]
impl OggStreamWriter {
    fn new(sample_rate: u32) -> Result<Self> {
        let normalized = match sample_rate {
            8000 | 12000 | 16000 | 24000 | 48000 => sample_rate,
            _ => 16000,
        };

        let encoder = {
            let mut error: c_int = 0;
            let ptr = unsafe {
                opus_encoder_create(
                    normalized as c_int,
                    2,
                    OPUS_APPLICATION_AUDIO,
                    &mut error as *mut c_int,
                )
            };

            if error != OPUS_OK {
                unsafe {
                    if !ptr.is_null() {
                        opus_encoder_destroy(ptr);
                    }
                }
                return Err(anyhow!(
                    "Failed to create Opus encoder: {}",
                    opus_error_message(error)
                ));
            }

            NonNull::new(ptr)
                .ok_or_else(|| anyhow!("Failed to create Opus encoder: null pointer returned"))?
        };

        let mut serial = rand::random::<u32>();
        if serial == 0 {
            serial = 1;
        }

        Ok(Self {
            encoder,
            serial,
            sequence: 0,
            granule_position: 0,
            sample_rate: normalized,
        })
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn granule_increment(&self, frame_samples: usize) -> u64 {
        let factor = 48000 / self.sample_rate;
        (frame_samples as u64) * (factor as u64)
    }

    fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        if pcm.len() % 2 != 0 {
            return Err(anyhow!(
                "PCM frame must contain an even number of samples for stereo Opus encoding"
            ));
        }

        let frame_size = (pcm.len() / 2) as c_int;
        let mut buffer = vec![0u8; 4096];
        let len = unsafe {
            opus_encode(
                self.encoder.as_ptr(),
                pcm.as_ptr() as *const opusic_sys::opus_int16,
                frame_size,
                buffer.as_mut_ptr(),
                buffer.len() as c_int,
            )
        };

        if len < 0 {
            return Err(anyhow!(
                "Failed to encode Opus frame: {}",
                opus_error_message(len)
            ));
        }

        buffer.truncate(len as usize);
        Ok(buffer)
    }

    async fn write_headers(&mut self, file: &mut File) -> Result<()> {
        let head = Self::build_opus_head(self.sample_rate);
        self.write_page(file, &head, 0, 0x02).await?;

        let tags = Self::build_opus_tags();
        self.write_page(file, &tags, 0, 0x00).await?;
        Ok(())
    }

    async fn write_audio_packet(
        &mut self,
        file: &mut File,
        packet: &[u8],
        frame_samples: usize,
    ) -> Result<()> {
        let increment = self.granule_increment(frame_samples);
        self.granule_position = self.granule_position.saturating_add(increment);
        self.write_page(file, packet, self.granule_position, 0x00)
            .await
    }

    async fn finalize(&mut self, file: &mut File) -> Result<()> {
        self.write_page(file, &[], self.granule_position, 0x04)
            .await
    }

    async fn write_page(
        &mut self,
        file: &mut File,
        packet: &[u8],
        granule_pos: u64,
        header_type: u8,
    ) -> Result<()> {
        let mut segments = Vec::new();
        if !packet.is_empty() {
            let mut remaining = packet.len();
            while remaining >= 255 {
                segments.push(255u8);
                remaining -= 255;
            }
            segments.push(remaining as u8);
        }

        let mut page = Vec::with_capacity(27 + segments.len() + packet.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // version
        page.push(header_type);
        page.extend_from_slice(&granule_pos.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // checksum placeholder
        page.push(segments.len() as u8);
        page.extend_from_slice(&segments);
        page.extend_from_slice(packet);

        let checksum = ogg_crc32(&page);
        page[22..26].copy_from_slice(&checksum.to_le_bytes());

        file.write_all(&page).await?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }

    fn build_opus_head(sample_rate: u32) -> Vec<u8> {
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(2); // channel count (stereo)
        head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&sample_rate.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family
        head
    }

    fn build_opus_tags() -> Vec<u8> {
        const VENDOR: &str = "rustpbx";
        let vendor_bytes = VENDOR.as_bytes();
        let mut tags = Vec::with_capacity(8 + 4 + vendor_bytes.len() + 4);
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor_bytes.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor_bytes);
        tags.extend_from_slice(&0u32.to_le_bytes()); // user comment list length
        tags
    }
}

#[cfg(feature = "opus")]
impl Drop for OggStreamWriter {
    fn drop(&mut self) {
        unsafe {
            opus_encoder_destroy(self.encoder.as_ptr());
        }
    }
}

#[cfg(feature = "opus")]
unsafe impl Send for OggStreamWriter {}

#[cfg(feature = "opus")]
unsafe impl Sync for OggStreamWriter {}

#[cfg(feature = "opus")]
fn ogg_crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0x04C11DB7;
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

impl RecorderFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            RecorderFormat::Wav => "wav",
            RecorderFormat::Ogg => "ogg",
        }
    }

    pub fn is_supported(&self) -> bool {
        match self {
            RecorderFormat::Wav => true,
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

pub struct Recorder {
    session_id: String,
    option: RecorderOption,
    samples_written: AtomicUsize,
    cancel_token: CancellationToken,
    channel_idx: AtomicUsize,
    channels: Mutex<HashMap<String, usize>>,
    stereo_buf: Mutex<PcmBuf>,
    mono_buf: Mutex<PcmBuf>,
}

impl Recorder {
    pub fn new(
        cancel_token: CancellationToken,
        session_id: String,
        option: RecorderOption,
    ) -> Self {
        Self {
            session_id,
            option,
            samples_written: AtomicUsize::new(0),
            cancel_token,
            channel_idx: AtomicUsize::new(0),
            channels: Mutex::new(HashMap::new()),
            stereo_buf: Mutex::new(Vec::new()),
            mono_buf: Mutex::new(Vec::new()),
        }
    }

    async fn update_wav_header(&self, file: &mut File) -> Result<()> {
        // Get total data size (in bytes)
        let total_samples = self.samples_written.load(Ordering::SeqCst);
        let data_size = total_samples * 4; // Stereo, 16-bit = 4 bytes per sample

        // Create a WavSpec for the WAV header
        let spec = WavSpec {
            channels: 2,
            sample_rate: self.option.samplerate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        // Create a memory buffer for the WAV header
        let mut header_buf = Vec::new();

        // Create a WAV header using standard structure
        // RIFF header
        header_buf.extend_from_slice(b"RIFF");
        let file_size = data_size + 36; // 36 bytes for header - 8 + data bytes
        header_buf.extend_from_slice(&(file_size as u32).to_le_bytes());
        header_buf.extend_from_slice(b"WAVE");

        // fmt subchunk - use values from WavSpec
        header_buf.extend_from_slice(b"fmt ");
        header_buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        header_buf.extend_from_slice(&1u16.to_le_bytes()); // PCM format
        header_buf.extend_from_slice(&(spec.channels as u16).to_le_bytes());
        header_buf.extend_from_slice(&(spec.sample_rate).to_le_bytes());

        // Bytes per second: sample_rate * num_channels * bytes_per_sample
        let bytes_per_sec =
            spec.sample_rate * (spec.channels as u32) * (spec.bits_per_sample as u32 / 8);
        header_buf.extend_from_slice(&bytes_per_sec.to_le_bytes());

        // Block align: num_channels * bytes_per_sample
        let block_align = (spec.channels as u16) * (spec.bits_per_sample / 8);
        header_buf.extend_from_slice(&block_align.to_le_bytes());
        header_buf.extend_from_slice(&spec.bits_per_sample.to_le_bytes());

        // Data subchunk
        header_buf.extend_from_slice(b"data");
        header_buf.extend_from_slice(&(data_size as u32).to_le_bytes());

        // Seek to beginning of file and write header
        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.write_all(&header_buf).await?;

        // Seek back to end of file for further writing
        file.seek(std::io::SeekFrom::End(0)).await?;

        Ok(())
    }

    pub async fn process_recording(
        &self,
        file_path: &Path,
        receiver: UnboundedReceiver<AudioFrame>,
    ) -> Result<()> {
        let requested_format = self.option.format.unwrap_or(RecorderFormat::Wav);
        let effective_format = requested_format.effective();

        if requested_format != effective_format {
            warn!(
                session_id = self.session_id,
                requested = requested_format.extension(),
                "Recorder format requires unavailable feature; falling back to wav"
            );
        }

        if effective_format == RecorderFormat::Ogg {
            #[cfg(feature = "opus")]
            {
                return self.process_recording_ogg(file_path, receiver).await;
            }
            #[cfg(not(feature = "opus"))]
            {
                unreachable!(
                    "RecorderFormat::effective() should prevent ogg when opus feature is disabled"
                );
            }
        }

        self.process_recording_wav(file_path, receiver).await
    }

    fn ensure_parent_dir(&self, file_path: &Path) -> Result<()> {
        if let Some(parent) = file_path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    warn!(
                        "Failed to create recording file parent directory: {} {}",
                        e,
                        file_path.display()
                    );
                    return Err(anyhow!("Failed to create recording file parent directory"));
                }
            }
        }
        Ok(())
    }

    async fn create_output_file(&self, file_path: &Path) -> Result<File> {
        self.ensure_parent_dir(file_path)?;
        match File::create(file_path).await {
            Ok(file) => {
                info!(
                    session_id = self.session_id,
                    "recorder: created recording file: {}",
                    file_path.display()
                );
                Ok(file)
            }
            Err(e) => {
                warn!(
                    "Failed to create recording file: {} {}",
                    e,
                    file_path.display()
                );
                Err(anyhow!("Failed to create recording file"))
            }
        }
    }

    async fn process_recording_wav(
        &self,
        file_path: &Path,
        mut receiver: UnboundedReceiver<AudioFrame>,
    ) -> Result<()> {
        let mut file = self.create_output_file(file_path).await?;
        self.update_wav_header(&mut file).await?;
        let chunk_size = (self.option.samplerate / 1000 * self.option.ptime) as usize;
        info!(
            session_id = self.session_id,
            format = "wav",
            "Recording to {} ptime: {}ms chunk_size: {}",
            file_path.display(),
            self.option.ptime,
            chunk_size
        );

        let mut interval = IntervalStream::new(tokio::time::interval(Duration::from_millis(
            self.option.ptime as u64,
        )));
        loop {
            select! {
                Some(frame) = receiver.recv() => {
                    self.append_frame(frame).await.ok();
                }
                _ = interval.next() => {
                    let (mono_buf, stereo_buf) = self.pop(chunk_size).await;
                    self.process_buffers(&mut file, mono_buf, stereo_buf).await?;
                    self.update_wav_header(&mut file).await?;
                }
                _ = self.cancel_token.cancelled() => {
                    self.flush_buffers(&mut file).await?;
                    self.update_wav_header(&mut file).await?;
                    return Ok(());
                }
            }
        }
    }

    #[cfg(feature = "opus")]
    async fn process_recording_ogg(
        &self,
        file_path: &Path,
        mut receiver: UnboundedReceiver<AudioFrame>,
    ) -> Result<()> {
        let mut file = self.create_output_file(file_path).await?;
        let mut writer = OggStreamWriter::new(self.option.samplerate)?;
        if writer.sample_rate() != self.option.samplerate {
            warn!(
                session_id = self.session_id,
                requested = self.option.samplerate,
                using = writer.sample_rate(),
                "Adjusted recorder samplerate to Opus-compatible value"
            );
        }
        writer.write_headers(&mut file).await?;

        let chunk_size = (self.option.samplerate / 1000 * self.option.ptime) as usize;
        info!(
            session_id = self.session_id,
            format = "ogg",
            "Recording to {} ptime: {}ms chunk_size: {}",
            file_path.display(),
            self.option.ptime,
            chunk_size
        );

        let frame_samples = std::cmp::max(1, (writer.sample_rate() / 50) as usize);
        let frame_step = frame_samples * 2; // stereo samples
        let mut pending: Vec<i16> = Vec::new();

        let mut interval = IntervalStream::new(tokio::time::interval(Duration::from_millis(
            self.option.ptime as u64,
        )));

        loop {
            select! {
                Some(frame) = receiver.recv() => {
                    self.append_frame(frame).await.ok();
                }
                _ = interval.next() => {
                    let (mono_buf, stereo_buf) = self.pop(chunk_size).await;
                    if mono_buf.is_empty() && stereo_buf.is_empty() {
                        continue;
                    }

                    let mix = Self::mix_buffers(&mono_buf, &stereo_buf);
                    pending.extend_from_slice(&mix);

                    let encoded_samples = self
                        .encode_pending_frames(&mut pending, frame_step, &mut writer, &mut file, false)
                        .await?;
                    if encoded_samples > 0 {
                        self.samples_written.fetch_add(encoded_samples, Ordering::SeqCst);
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    let (mono_buf, stereo_buf) = self.pop(usize::MAX).await;
                    if !mono_buf.is_empty() || !stereo_buf.is_empty() {
                        let mix = Self::mix_buffers(&mono_buf, &stereo_buf);
                        pending.extend_from_slice(&mix);
                    }

                    let encoded_samples = self
                        .encode_pending_frames(&mut pending, frame_step, &mut writer, &mut file, true)
                        .await?;
                    if encoded_samples > 0 {
                        self.samples_written.fetch_add(encoded_samples, Ordering::SeqCst);
                    }

                    writer.finalize(&mut file).await?;
                    return Ok(());
                }
            }
        }
    }

    #[cfg(feature = "opus")]
    async fn encode_pending_frames(
        &self,
        pending: &mut Vec<i16>,
        frame_step: usize,
        writer: &mut OggStreamWriter,
        file: &mut File,
        pad_final: bool,
    ) -> Result<usize> {
        let mut total_samples = 0usize;
        let samples_per_channel = frame_step / 2;
        while pending.len() >= frame_step {
            let frame: Vec<i16> = pending.drain(..frame_step).collect();
            let packet = writer.encode_frame(&frame)?;
            writer
                .write_audio_packet(file, &packet, samples_per_channel)
                .await?;
            total_samples += samples_per_channel;
        }

        if pad_final && !pending.is_empty() {
            let mut frame: Vec<i16> = pending.drain(..).collect();
            frame.resize(frame_step, 0);
            let packet = writer.encode_frame(&frame)?;
            writer
                .write_audio_packet(file, &packet, samples_per_channel)
                .await?;
            total_samples += samples_per_channel;
        }

        Ok(total_samples)
    }

    /// Get or assign channel index for a track
    fn get_channel_index(&self, track_id: &str) -> usize {
        let mut channels = self.channels.lock().unwrap();
        if let Some(&channel_idx) = channels.get(track_id) {
            channel_idx % 2
        } else {
            let new_idx = self.channel_idx.fetch_add(1, Ordering::SeqCst);
            channels.insert(track_id.to_string(), new_idx);
            info!(
                session_id = self.session_id,
                "Assigned channel {} to track: {}",
                new_idx % 2,
                track_id
            );
            new_idx % 2
        }
    }

    async fn append_frame(&self, frame: AudioFrame) -> Result<()> {
        let buffer = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => return Ok(()), // ignore non-PCM frames
        };

        // Validate audio data
        if buffer.is_empty() {
            return Ok(());
        }

        // Get channel assignment
        let channel_idx = self.get_channel_index(&frame.track_id);

        // Add to appropriate buffer
        match channel_idx {
            0 => {
                let mut mono_buf = self.mono_buf.lock().unwrap();
                mono_buf.extend(buffer.iter());
            }
            1 => {
                let mut stereo_buf = self.stereo_buf.lock().unwrap();
                stereo_buf.extend(buffer.iter());
            }
            _ => {}
        }

        Ok(())
    }

    /// Extract samples from a buffer without padding
    pub(crate) fn extract_samples(buffer: &mut PcmBuf, extract_size: usize) -> PcmBuf {
        if extract_size > 0 && !buffer.is_empty() {
            let take_size = extract_size.min(buffer.len());
            buffer.drain(..take_size).collect()
        } else {
            Vec::new()
        }
    }

    async fn pop(&self, chunk_size: usize) -> (PcmBuf, PcmBuf) {
        let mut mono_buf = self.mono_buf.lock().unwrap();
        let mut stereo_buf = self.stereo_buf.lock().unwrap();

        // Limit chunk_size to prevent capacity overflow
        let safe_chunk_size = chunk_size.min(16000 * 10); // Max 10 seconds at 16kHz

        let mono_result = if mono_buf.len() >= safe_chunk_size {
            // Sufficient data, extract complete chunk
            Self::extract_samples(&mut mono_buf, safe_chunk_size)
        } else if !mono_buf.is_empty() {
            // Partial data, extract all and pad with silence
            let available_len = mono_buf.len(); // Store length before mutable borrow
            let mut result = Self::extract_samples(&mut mono_buf, available_len);
            if chunk_size != usize::MAX {
                // Don't pad when flushing
                result.resize(safe_chunk_size, 0); // Pad with silence to chunk_size
            }
            result
        } else {
            // No data, output silence (only when not flushing)
            if chunk_size != usize::MAX {
                vec![0; safe_chunk_size]
            } else {
                Vec::new()
            }
        };

        let stereo_result = if stereo_buf.len() >= safe_chunk_size {
            // Sufficient data, extract complete chunk
            Self::extract_samples(&mut stereo_buf, safe_chunk_size)
        } else if !stereo_buf.is_empty() {
            // Partial data, extract all and pad with silence
            let available_len = stereo_buf.len(); // Store length before mutable borrow
            let mut result = Self::extract_samples(&mut stereo_buf, available_len);
            if chunk_size != usize::MAX {
                // Don't pad when flushing
                result.resize(safe_chunk_size, 0); // Pad with silence to chunk_size
            }
            result
        } else {
            // No data, output silence (only when not flushing)
            if chunk_size != usize::MAX {
                vec![0; safe_chunk_size]
            } else {
                Vec::new()
            }
        };

        // Ensure buffers have equal length when flushing
        if chunk_size == usize::MAX {
            let max_len = mono_result.len().max(stereo_result.len());
            let mut mono_final = mono_result;
            let mut stereo_final = stereo_result;
            mono_final.resize(max_len, 0);
            stereo_final.resize(max_len, 0);
            (mono_final, stereo_final)
        } else {
            (mono_result, stereo_result)
        }
    }

    pub fn stop_recording(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    /// Mix mono and stereo buffers into interleaved stereo output
    pub(crate) fn mix_buffers(mono_buf: &PcmBuf, stereo_buf: &PcmBuf) -> Vec<i16> {
        // Ensure both buffers have equal length (guaranteed by pop() method)
        assert_eq!(
            mono_buf.len(),
            stereo_buf.len(),
            "Buffer lengths must be equal after pop()"
        );

        let len = mono_buf.len();
        let mut mix_buff = Vec::with_capacity(len * 2);

        for i in 0..len {
            mix_buff.push(mono_buf[i]); // Left channel
            mix_buff.push(stereo_buf[i]); // Right channel
        }

        mix_buff
    }

    /// Write mixed audio data to file
    async fn write_audio_data(
        &self,
        file: &mut File,
        mono_buf: &PcmBuf,
        stereo_buf: &PcmBuf,
    ) -> Result<usize> {
        let max_len = mono_buf.len().max(stereo_buf.len());
        if max_len == 0 {
            return Ok(0);
        }

        let mix_buff = Self::mix_buffers(mono_buf, stereo_buf);

        file.seek(std::io::SeekFrom::End(0)).await?;
        file.write_all(&samples_to_bytes(&mix_buff)).await?;

        Ok(max_len)
    }

    /// Process buffers with quality checks and write to file
    async fn process_buffers(
        &self,
        file: &mut File,
        mono_buf: PcmBuf,
        stereo_buf: PcmBuf,
    ) -> Result<()> {
        // Skip if no data
        if mono_buf.is_empty() && stereo_buf.is_empty() {
            return Ok(());
        }
        // Write audio data
        let samples_written = self.write_audio_data(file, &mono_buf, &stereo_buf).await?;
        if samples_written > 0 {
            self.samples_written
                .fetch_add(samples_written, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Flush all remaining buffer content
    async fn flush_buffers(&self, file: &mut File) -> Result<()> {
        loop {
            let (mono_buf, stereo_buf) = self.pop(usize::MAX).await;

            if mono_buf.is_empty() && stereo_buf.is_empty() {
                break;
            }

            let samples_written = self.write_audio_data(file, &mono_buf, &stereo_buf).await?;
            if samples_written > 0 {
                self.samples_written
                    .fetch_add(samples_written, Ordering::SeqCst);
            }
        }

        Ok(())
    }
}
