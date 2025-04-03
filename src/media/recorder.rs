use crate::{media::codecs::convert_s16_to_u8, AudioFrame, Samples};
use anyhow::Result;
use byteorder::{ByteOrder, LittleEndian};
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

#[derive(Debug, Deserialize, Serialize)]
pub struct RecorderConfig {
    pub samplerate: u32,
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

        // Create WAV header
        let mut header = vec![0u8; 44];

        // Write RIFF header
        header[0..4].copy_from_slice(b"RIFF");
        LittleEndian::write_u32(&mut header[4..8], 36 + data_size as u32); // File size - 8
        header[8..12].copy_from_slice(b"WAVE");

        // Write format chunk
        header[12..16].copy_from_slice(b"fmt ");
        LittleEndian::write_u32(&mut header[16..20], 16); // Format chunk size
        LittleEndian::write_u16(&mut header[20..22], 1); // PCM format
        LittleEndian::write_u16(&mut header[22..24], 2); // 2 channels (stereo)
        LittleEndian::write_u32(&mut header[24..28], self.config.samplerate); // Sample rate
        LittleEndian::write_u32(&mut header[28..32], self.config.samplerate * 4); // Byte rate
        LittleEndian::write_u16(&mut header[32..34], 4); // Block align
        LittleEndian::write_u16(&mut header[34..36], 16); // Bits per sample

        // Write data chunk
        header[36..40].copy_from_slice(b"data");
        LittleEndian::write_u32(&mut header[40..44], data_size as u32); // Data size

        // Seek to beginning of file and write header
        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.write_all(&header).await?;

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

        // Write initial WAV header
        // Define spec for reference only (not directly used)
        let _spec = WavSpec {
            channels: 2,
            sample_rate: self.config.samplerate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        // Write placeholder header (will be updated later)
        let header_size = 44;
        let placeholder = vec![0u8; header_size];
        file.write_all(&placeholder).await?;

        info!("Recording to {}", file_path.display());

        let mut interval = IntervalStream::new(tokio::time::interval(self.config.ptime));
        let chunk_size =
            (self.config.samplerate / 1000 * self.config.ptime.as_millis() as u32) as usize;

        loop {
            let mut count: u32 = 0;
            select! {
                Some(_) = interval.next() => {
                    while let Ok(frame) = receiver.try_recv() {
                        self.append_frame(frame).await.ok();
                    }

                    let (mono_buf, stereo_buf) = self.pop().await.unwrap_or((vec![0; chunk_size], vec![0; chunk_size as usize]));
                    let mut mix_buff = vec![0; chunk_size * 2]; // Doubled size for stereo interleaving
                    for i in 0..mono_buf.len().min(chunk_size) {
                        mix_buff[i * 2] = mono_buf[i];
                    }
                    for i in 0..stereo_buf.len().min(chunk_size) {
                        mix_buff[i * 2 + 1] = stereo_buf[i];
                    }

                    file.write_all(&convert_s16_to_u8(&mix_buff)).await?;
                    count += 1;
                    if count % 5 == 0 {
                         // update header every 100ms
                        self.update_wav_header(&mut file).await?;
                    }
                    self.samples_written.fetch_add(chunk_size, Ordering::SeqCst);
                }
                _ = self.cancel_token.cancelled() => {
                    // Update the final header
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

    async fn pop(&self) -> Option<(Vec<i16>, Vec<i16>)> {
        let mut mono_buf = self.mono_buf.lock().unwrap();
        let mut stereo_buf = self.stereo_buf.lock().unwrap();

        // Return non-empty buffers even if one is empty
        if mono_buf.len() > 0 || stereo_buf.len() > 0 {
            // take all buffers
            let mono_buf = mono_buf.drain(..).collect();
            let stereo_buf = stereo_buf.drain(..).collect();
            Some((mono_buf, stereo_buf))
        } else {
            None
        }
    }

    pub fn stop_recording(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }
}
