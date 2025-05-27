use crate::{media::codecs::samples_to_bytes, AudioFrame, PcmBuf, Samples};
use anyhow::Result;
use futures::StreamExt;
use hound::{SampleFormat, WavSpec};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
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

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct RecorderOption {
    pub recorder_file: String,
    pub samplerate: u32,
    #[serde(skip)]
    pub ptime: Duration,
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
            recorder_file: "./recordings.wav".to_string(),
            samplerate: 16000,
            ptime: Duration::from_millis(200),
        }
    }
}

pub struct Recorder {
    option: RecorderOption,
    samples_written: AtomicUsize,
    cancel_token: CancellationToken,
    channel_idx: AtomicUsize,
    channels: Mutex<HashMap<String, usize>>,
    stereo_buf: Mutex<PcmBuf>,
    mono_buf: Mutex<PcmBuf>,
}

impl Recorder {
    pub fn new(cancel_token: CancellationToken, option: RecorderOption) -> Self {
        Self {
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
        mut receiver: UnboundedReceiver<AudioFrame>,
    ) -> Result<()> {
        let mut file = match File::create(file_path).await {
            Ok(file) => file,
            Err(e) => {
                warn!(
                    "Failed to create recording file: {} {}",
                    e,
                    file_path.display()
                );
                return Err(anyhow::anyhow!("Failed to create recording file"));
            }
        };
        info!("recorder: created recording file: {}", file_path.display());
        // Create an initial WAV header
        self.update_wav_header(&mut file).await?;
        let chunk_size =
            (self.option.samplerate / 1000 * self.option.ptime.as_millis() as u32) as usize;
        info!(
            "Recording to {} ptime: {}ms chunk_size: {}",
            file_path.display(),
            self.option.ptime.as_millis(),
            chunk_size
        );

        let mut interval = IntervalStream::new(tokio::time::interval(self.option.ptime));
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
                    // Flush remaining buffer content before exiting
                    self.flush_buffers(&mut file).await?;

                    // Update the final header before finishing
                    self.update_wav_header(&mut file).await?;
                    info!("Recording stopped, final header updated");
                    return Ok(());
                }
            }
        }
    }

    /// Get or assign channel index for a track
    fn get_channel_index(&self, track_id: &str) -> usize {
        let mut channels = self.channels.lock().unwrap();
        if let Some(&channel_idx) = channels.get(track_id) {
            channel_idx % 2
        } else {
            let new_idx = self.channel_idx.fetch_add(1, Ordering::SeqCst);
            channels.insert(track_id.to_string(), new_idx);
            info!("Assigned channel {} to track: {}", new_idx % 2, track_id);
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
    fn extract_samples(buffer: &mut PcmBuf, extract_size: usize) -> PcmBuf {
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

        // Calculate how much data we can extract without padding
        let mono_available = mono_buf.len();
        let stereo_available = stereo_buf.len();

        // Take the minimum of available data and chunk_size, but at least some data if available
        let extract_size = if mono_available > 0 || stereo_available > 0 {
            let max_available = mono_available.max(stereo_available);
            max_available.min(chunk_size).max(1) // At least 1 sample if any data available
        } else {
            0 // No data available at all
        };

        let mono_result = Self::extract_samples(&mut mono_buf, extract_size);
        let stereo_result = Self::extract_samples(&mut stereo_buf, extract_size);

        (mono_result, stereo_result)
    }

    pub fn stop_recording(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    /// Detect audio clipping
    fn detect_clipping(samples: &PcmBuf) -> bool {
        const CLIP_THRESHOLD: i16 = 32760; // Threshold close to maximum value
        samples.iter().any(|&sample| sample.abs() >= CLIP_THRESHOLD)
    }

    /// Detect consecutive identical values (freeze detection)
    fn detect_constant_value(samples: &PcmBuf) -> bool {
        if samples.len() < 50 {
            // Increase minimum length threshold further
            return false;
        }

        let first_value = samples[0];

        // For zero values (silence), use much higher threshold
        if first_value == 0 {
            // Only warn about very large zero buffers that might indicate data flow issues
            let all_zeros = samples.iter().all(|&sample| sample == 0);
            return all_zeros && samples.len() > 1600; // Only warn for >100ms of continuous silence at 16kHz
        }

        let consecutive_count = samples
            .iter()
            .take_while(|&&sample| sample == first_value)
            .count();

        // Keep stricter threshold for non-zero constant values (actual hardware issues)
        consecutive_count >= samples.len().min(50) // 50 or more consecutive identical non-zero values
    }

    /// Mix mono and stereo buffers into interleaved stereo output
    fn mix_buffers(mono_buf: &PcmBuf, stereo_buf: &PcmBuf) -> Vec<i16> {
        let max_len = mono_buf.len().max(stereo_buf.len());
        let mut mix_buff = vec![0; max_len * 2];

        for i in 0..max_len {
            let mono_sample = mono_buf.get(i).copied().unwrap_or(0);
            let stereo_sample = stereo_buf.get(i).copied().unwrap_or(0);

            mix_buff[i * 2] = mono_sample; // Left channel
            mix_buff[i * 2 + 1] = stereo_sample; // Right channel
        }

        mix_buff
    }

    /// Perform audio quality checks on buffers
    fn check_audio_quality(&self, mono_buf: &PcmBuf, stereo_buf: &PcmBuf) {
        // Check clipping
        if !mono_buf.is_empty() && Self::detect_clipping(mono_buf) {
            warn!("Audio clipping detected: mono=true");
        }

        if !stereo_buf.is_empty() && Self::detect_clipping(stereo_buf) {
            warn!("Audio clipping detected: stereo=true");
        }

        // Check constant values
        let mono_constant = !mono_buf.is_empty() && Self::detect_constant_value(mono_buf);
        let stereo_constant = !stereo_buf.is_empty() && Self::detect_constant_value(stereo_buf);

        if mono_constant || stereo_constant {
            let mono_info = if mono_constant {
                let first_val = mono_buf.first().unwrap_or(&0);
                format!("mono(value={}, len={})", first_val, mono_buf.len())
            } else {
                "mono(ok)".to_string()
            };

            let stereo_info = if stereo_constant {
                let first_val = stereo_buf.first().unwrap_or(&0);
                format!("stereo(value={}, len={})", first_val, stereo_buf.len())
            } else {
                "stereo(ok)".to_string()
            };

            warn!("Constant value detected: {} {}", mono_info, stereo_info);
        }
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

        // Quality checks
        self.check_audio_quality(&mono_buf, &stereo_buf);

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
        info!("Flushing remaining buffer content before stopping...");

        loop {
            let (mono_buf, stereo_buf) = self.pop(usize::MAX).await;

            if mono_buf.is_empty() && stereo_buf.is_empty() {
                break;
            }

            let samples_written = self.write_audio_data(file, &mono_buf, &stereo_buf).await?;
            if samples_written > 0 {
                self.samples_written
                    .fetch_add(samples_written, Ordering::SeqCst);
                info!("Flushed {} samples", samples_written);
            }
        }

        Ok(())
    }
}
