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
            ptime: Duration::from_millis(20),
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

        let mut count: u32 = 0;
        loop {
            select! {
                Some(frame) = receiver.recv() => {
                    self.append_frame(frame).await.ok();
                }
                _ = interval.next() => {
                    let (mono_buf, stereo_buf) = self.pop(chunk_size).await;

                    // Verify data integrity
                    if mono_buf.len() != chunk_size || stereo_buf.len() != chunk_size {
                        warn!("Buffer size mismatch: mono={}, stereo={}, expected={}",
                              mono_buf.len(), stereo_buf.len(), chunk_size);
                    }

                    // Detect clipping
                    let mono_clipped = Self::detect_clipping(&mono_buf);
                    let stereo_clipped = Self::detect_clipping(&stereo_buf);
                    if mono_clipped || stereo_clipped {
                        warn!("Audio clipping detected: mono={}, stereo={}", mono_clipped, stereo_clipped);
                    }

                    // Detect constant values (freeze detection)
                    let mono_constant = Self::detect_constant_value(&mono_buf);
                    let stereo_constant = Self::detect_constant_value(&stereo_buf);
                    if mono_constant || stereo_constant {
                        warn!("Constant value detected (possible freeze): mono={}, stereo={}",
                              mono_constant, stereo_constant);
                    }

                    let max_len = mono_buf.len().max(stereo_buf.len());
                    let mut mix_buff = vec![0; max_len * 2];

                    // Improved mixing logic with proper data alignment
                    for i in 0..max_len {
                        let mono_sample = if i < mono_buf.len() { mono_buf[i] } else { 0 };
                        let stereo_sample = if i < stereo_buf.len() { stereo_buf[i] } else { 0 };

                        mix_buff[i * 2] = mono_sample;      // Left channel
                        mix_buff[i * 2 + 1] = stereo_sample; // Right channel
                    }

                    file.seek(std::io::SeekFrom::End(0)).await?;
                    file.write_all(&samples_to_bytes(&mix_buff)).await?;

                    self.samples_written.fetch_add(max_len as usize, Ordering::SeqCst);
                    count += 1;

                    if count % 5 == 0 {
                        self.update_wav_header(&mut file).await?;
                    }
                }
                _ = self.cancel_token.cancelled() => {
                    // Update the final header before finishing
                    self.update_wav_header(&mut file).await?;
                    return Ok(());
                }
            }
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

        // Detect abnormal data
        if Self::detect_clipping(&buffer) {
            warn!("Clipping detected in frame from track: {}", frame.track_id);
        }

        if Self::detect_constant_value(&buffer) {
            warn!(
                "Constant value detected in frame from track: {}",
                frame.track_id
            );
        }

        let mut channels = self.channels.lock().unwrap();
        let channel_idx = if let Some(&channel_idx) = channels.get(&frame.track_id) {
            channel_idx % 2
        } else {
            let new_idx = self.channel_idx.fetch_add(1, Ordering::SeqCst);
            channels.insert(frame.track_id.clone(), new_idx);
            info!(
                "Assigned channel {} to track: {}",
                new_idx % 2,
                frame.track_id
            );
            new_idx % 2
        };

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

    async fn pop(&self, chunk_size: usize) -> (PcmBuf, PcmBuf) {
        let mut mono_buf = self.mono_buf.lock().unwrap();
        let mut stereo_buf = self.stereo_buf.lock().unwrap();

        let mono_result = if mono_buf.len() >= chunk_size {
            // Sufficient data available, extract normally
            mono_buf.drain(..chunk_size).collect()
        } else if !mono_buf.is_empty() {
            // Insufficient data but some available, extract all and pad with zeros
            let mut result = mono_buf.drain(..).collect::<Vec<_>>();
            result.resize(chunk_size, 0);
            result
        } else {
            // No data available, fill with zeros
            vec![0; chunk_size]
        };

        let stereo_result = if stereo_buf.len() >= chunk_size {
            // Sufficient data available, extract normally
            stereo_buf.drain(..chunk_size).collect()
        } else if !stereo_buf.is_empty() {
            // Insufficient data but some available, extract all and pad with zeros
            let mut result = stereo_buf.drain(..).collect::<Vec<_>>();
            result.resize(chunk_size, 0);
            result
        } else {
            // No data available, fill with zeros
            vec![0; chunk_size]
        };

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
        if samples.len() < 10 {
            return false;
        }

        let first_value = samples[0];
        let consecutive_count = samples
            .iter()
            .take_while(|&&sample| sample == first_value)
            .count();

        consecutive_count >= samples.len().min(10) // 10 or more consecutive identical values
    }
}
