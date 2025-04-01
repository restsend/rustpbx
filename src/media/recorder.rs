use crate::{AudioFrame, Samples};
use anyhow::Result;
use byteorder::{ByteOrder, LittleEndian};
use hound::{SampleFormat, WavSpec};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::mpsc::UnboundedReceiver,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct RecorderConfig {
    pub sample_rate: u32,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        Self { sample_rate: 16000 }
    }
}

pub struct Recorder {
    config: RecorderConfig,
    samples_written: AtomicUsize,
    cancel_token: CancellationToken,
    last_header_update: Arc<Mutex<Instant>>,
    channel_buffers: Arc<Mutex<HashMap<String, Vec<i16>>>>,
}

impl Recorder {
    pub fn new(cancel_token: CancellationToken, config: RecorderConfig) -> Self {
        Self {
            config,
            samples_written: AtomicUsize::new(0),
            cancel_token,
            last_header_update: Arc::new(Mutex::new(Instant::now())),
            channel_buffers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn write_frame(&self, file: &mut File, frame: AudioFrame) -> Result<()> {
        match frame.samples {
            Samples::PCM(samples) => {
                let track_id = frame.track_id.clone();

                // Store samples in appropriate channel buffer
                {
                    let mut buffers = self.channel_buffers.lock().unwrap();
                    let buffer = buffers.entry(track_id).or_insert_with(Vec::new);
                    buffer.extend_from_slice(&samples);
                }

                // Process buffered samples to write interleaved stereo
                let stereo_data = {
                    let buffers = self.channel_buffers.lock().unwrap();
                    if buffers.len() >= 2 {
                        let keys: Vec<String> = buffers.keys().cloned().collect();
                        if keys.len() >= 2 {
                            let left_channel = &buffers[&keys[0]];
                            let right_channel = &buffers[&keys[1]];

                            // Find the minimum length between channels
                            let min_len = std::cmp::min(left_channel.len(), right_channel.len());
                            if min_len > 0 {
                                // Create interleaved stereo data
                                let mut stereo_data = Vec::with_capacity(min_len * 2);
                                for i in 0..min_len {
                                    stereo_data.push(left_channel[i]);
                                    stereo_data.push(right_channel[i]);
                                }

                                // Remove processed samples from buffers
                                let mut buffers_mut = self.channel_buffers.lock().unwrap();
                                if let Some(left) = buffers_mut.get_mut(&keys[0]) {
                                    left.drain(0..min_len);
                                }
                                if let Some(right) = buffers_mut.get_mut(&keys[1]) {
                                    right.drain(0..min_len);
                                }

                                Some(stereo_data)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };

                // Write stereo data if available
                if let Some(stereo_data) = stereo_data {
                    // Convert i16 samples to bytes
                    let mut bytes = Vec::with_capacity(stereo_data.len() * 2);
                    for sample in &stereo_data {
                        let mut sample_bytes = [0u8; 2];
                        LittleEndian::write_i16(&mut sample_bytes, *sample);
                        bytes.extend_from_slice(&sample_bytes);
                    }

                    // Write the bytes to the file
                    file.write_all(&bytes).await?;

                    // Update number of samples written
                    self.samples_written
                        .fetch_add(stereo_data.len() / 2, Ordering::SeqCst);

                    // Update WAV header if needed (every 100ms)
                    let should_update = {
                        let mut last_update = self.last_header_update.lock().unwrap();
                        let now = Instant::now();
                        let elapsed = now.duration_since(*last_update);
                        if elapsed >= Duration::from_millis(100) {
                            *last_update = now;
                            true
                        } else {
                            false
                        }
                    };

                    if should_update {
                        self.update_wav_header(file).await?;
                    }
                }
            }
            _ => {
                // Non-PCM samples are not supported for recording
                info!("Non-PCM samples are not supported for recording");
            }
        }

        Ok(())
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
        LittleEndian::write_u32(&mut header[24..28], self.config.sample_rate); // Sample rate
        LittleEndian::write_u32(&mut header[28..32], self.config.sample_rate * 4); // Byte rate
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
        let mut file = File::create(file_path).await?;

        // Write initial WAV header
        // Define spec for reference only (not directly used)
        let _spec = WavSpec {
            channels: 2,
            sample_rate: self.config.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        // Write placeholder header (will be updated later)
        let header_size = 44;
        let placeholder = vec![0u8; header_size];
        file.write_all(&placeholder).await?;

        info!("Recording to {}", file_path.display());
        while let Some(frame) = receiver.recv().await {
            self.write_frame(&mut file, frame).await?;
        }

        // Final header update
        self.update_wav_header(&mut file).await?;

        Ok(())
    }

    pub fn stop_recording(&mut self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }
}
