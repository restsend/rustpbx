use crate::{media::codecs::convert_s16_to_u8, AudioFrame, Samples};
use anyhow::Result;
use byteorder::{ByteOrder, LittleEndian};
use futures::StreamExt;
use hound::{SampleFormat, WavSpec};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct RecorderConfig {
    pub samplerate: u32,
    #[serde(skip)]
    pub ptime: Duration,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self {
            samplerate: 16000,
            ptime: Duration::from_millis(20),
        }
    }
}

pub struct Recorder {
    config: RecorderConfig,
    samples_written: AtomicUsize,
    cancel_token: CancellationToken,
    channel_idx: AtomicUsize,
    channels: Mutex<HashMap<String, usize>>,
    stereo_buf: Mutex<Vec<i16>>,
    mono_buf: Mutex<Vec<i16>>,
}

impl Recorder {
    pub fn new(cancel_token: CancellationToken, config: RecorderConfig) -> Self {
        Self {
            config,
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
            sample_rate: self.config.samplerate,
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

        // Create an initial WAV header
        self.update_wav_header(&mut file).await?;
        let chunk_size =
            (self.config.samplerate / 1000 * self.config.ptime.as_millis() as u32) as usize;
        info!(
            "Recording to {} ptime: {}ms chunk_size: {}",
            file_path.display(),
            self.config.ptime.as_millis(),
            chunk_size
        );

        let mut interval = IntervalStream::new(tokio::time::interval(self.config.ptime));

        let mut count: u32 = 0;
        loop {
            select! {
                Some(_) = interval.next() => {
                    while let Ok(frame) = receiver.try_recv() {
                        self.append_frame(frame).await.ok();
                    }

                    let (mono_buf, stereo_buf) = self.pop(chunk_size).await.unwrap_or((vec![0; chunk_size], vec![0; chunk_size as usize]));
                    let max_len = mono_buf.len().max(stereo_buf.len());
                    let mut mix_buff = vec![0; max_len * 2]; // Doubled size for stereo interleaving
                    for i in 0..mono_buf.len() {
                        mix_buff[i * 2] = mono_buf[i];
                    }
                    for i in 0..stereo_buf.len() {
                        mix_buff[i * 2 + 1] = stereo_buf[i];
                    }
                    // Move to the end of file before writing audio data
                    file.seek(std::io::SeekFrom::End(0)).await?;
                    file.write_all(&convert_s16_to_u8(&mix_buff)).await?;

                    // Update the samples written counter
                    self.samples_written.fetch_add(max_len as usize, Ordering::SeqCst);
                    count += 1;

                    // Update header every 5 frames (approximately 100ms with 20ms ptime)
                    if count % 5 == 0 {
                        // Update the WAV header with current sample count
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
            Samples::PCM(samples) => samples,
            _ => return Ok(()), // ignore non-PCM frames
        };
        let mut channels = self.channels.lock().unwrap();
        let channel_idx = if let Some(channel_idx) = channels.get(&frame.track_id) {
            channel_idx % 2
        } else {
            self.channel_idx.fetch_add(1, Ordering::SeqCst);
            let new_idx = self.channel_idx.load(Ordering::SeqCst);
            channels.insert(frame.track_id, new_idx);
            new_idx % 2
        };
        match channel_idx {
            0 => self.mono_buf.lock().unwrap().extend(buffer.iter()),
            1 => self.stereo_buf.lock().unwrap().extend(buffer.iter()),
            _ => {}
        }
        Ok(())
    }

    async fn pop(&self, chunk_size: usize) -> Option<(Vec<i16>, Vec<i16>)> {
        let mut mono_buf = self.mono_buf.lock().unwrap();
        let mut stereo_buf = self.stereo_buf.lock().unwrap();

        let mono_buf = if mono_buf.len() > chunk_size {
            mono_buf.drain(..chunk_size).collect()
        } else {
            vec![0; chunk_size]
        };
        let stereo_buf = if stereo_buf.len() > chunk_size {
            stereo_buf.drain(..chunk_size).collect()
        } else {
            vec![0; chunk_size]
        };
        Some((mono_buf, stereo_buf))
    }

    pub fn stop_recording(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }
}
