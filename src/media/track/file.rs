use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hound::{WavReader, WavSpec};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::media::{
    processor::{AudioFrame, Processor},
    stream::EventSender,
    track::{Track, TrackConfig, TrackId, TrackPacket, TrackPacketSender, TrackPayload},
};

pub struct FileTrack {
    id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    processors: Vec<Box<dyn Processor>>,
    path: Option<String>,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<TrackPacket>>>>,
}

impl FileTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            config: TrackConfig::default(),
            cancel_token: CancellationToken::new(),
            processors: Vec::new(),
            path: None,
            receiver: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_path(mut self, path: String) -> Self {
        self.path = Some(path);
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);
        self
    }

    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.config = self.config.with_ptime(ptime);
        self
    }
}

#[async_trait]
impl Track for FileTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
    }

    fn processors(&self) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .map(|p| p.as_ref() as &dyn Processor)
            .collect()
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Create a channel for receiving packets
        let (sender, receiver) = mpsc::unbounded_channel();

        // Store the receiver in self (thread-safe)
        {
            let mut receiver_guard = self.receiver.lock().unwrap();
            *receiver_guard = Some(receiver);
        }

        // If we have a path, start streaming the WAV file
        if let Some(path) = &self.path {
            let path = path.clone();
            let id = self.id.clone();
            let sample_rate = self.config.sample_rate;
            let max_pcm_chunk_size = self.config.max_pcm_chunk_size;

            tokio::spawn(async move {
                let stream_result = stream_wav_file(
                    &path,
                    &id,
                    sample_rate,
                    max_pcm_chunk_size,
                    token.clone(),
                    packet_sender,
                )
                .await;

                if let Err(e) = stream_result {
                    tracing::error!("Error streaming WAV file: {}", e);
                }

                // Signal the end of the file
                let _ = event_sender.send(crate::media::stream::MediaStreamEvent::TrackStop(id));
            });
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Nothing to do here as the file streaming task will stop when the token is cancelled
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &TrackPacket) -> Result<()> {
        if let TrackPayload::PCM(samples) = &packet.payload {
            let mut frame = AudioFrame {
                track_id: packet.track_id.clone(),
                samples: samples.clone(),
                timestamp: packet.timestamp as u32,
                sample_rate: self.config.sample_rate as u16,
            };

            // Apply processors to the frame
            for processor in &self.processors {
                let _ = processor.process_frame(&mut frame);
            }
        }
        Ok(())
    }

    async fn recv_packet(&self) -> Option<TrackPacket> {
        let mut receiver_opt: Option<mpsc::UnboundedReceiver<TrackPacket>> = None;

        // Use a block to limit the mutex lock duration
        {
            let mut receiver_guard = self.receiver.lock().unwrap();
            if let Some(rec) = receiver_guard.as_mut() {
                // Try to receive a packet without blocking
                match rec.try_recv() {
                    Ok(packet) => return Some(packet),
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        *receiver_guard = None;
                    }
                }
            }
        }

        // If we didn't get a packet immediately, try with await
        if let Some(mut receiver) = receiver_opt {
            return receiver.recv().await;
        }

        None
    }
}

// Helper function to stream a WAV file
async fn stream_wav_file(
    path: &str,
    track_id: &str,
    target_sample_rate: u32,
    max_pcm_chunk_size: usize,
    token: CancellationToken,
    packet_sender: TrackPacketSender,
) -> Result<()> {
    // Open the WAV file
    let reader = BufReader::new(File::open(path)?);
    let mut wav_reader = WavReader::new(reader)?;

    let spec = wav_reader.spec();
    let duration_per_sample = 1.0 / spec.sample_rate as f64;

    // Calculate resampling ratio if needed
    let resample_ratio = target_sample_rate as f64 / spec.sample_rate as f64;

    // Read all samples (we'll do this in a blocking task since hound is not async)
    let samples_result = tokio::task::spawn_blocking(move || {
        let mut all_samples = Vec::new();

        match spec.sample_format {
            hound::SampleFormat::Int => {
                match spec.bits_per_sample {
                    16 => {
                        for sample in wav_reader.samples::<i16>() {
                            all_samples.push(sample.unwrap_or(0));
                        }
                    }
                    8 => {
                        for sample in wav_reader.samples::<i8>() {
                            all_samples.push(sample.unwrap_or(0) as i16);
                        }
                    }
                    24 => {
                        // Handle 24-bit as i32 but scale appropriately
                        for sample in wav_reader.samples::<i32>() {
                            all_samples.push((sample.unwrap_or(0) >> 16) as i16);
                        }
                    }
                    32 => {
                        for sample in wav_reader.samples::<i32>() {
                            all_samples.push((sample.unwrap_or(0) >> 16) as i16);
                        }
                    }
                    _ => {
                        return Err(anyhow!(
                            "Unsupported bits per sample: {}",
                            spec.bits_per_sample
                        ))
                    }
                }
            }
            hound::SampleFormat::Float => {
                for sample in wav_reader.samples::<f32>() {
                    all_samples.push((sample.unwrap_or(0.0) * 32767.0) as i16);
                }
            }
        }

        Ok((all_samples, spec))
    })
    .await??;

    let (mut all_samples, spec) = samples_result;

    // If stereo, convert to mono by averaging channels
    if spec.channels == 2 {
        let mono_samples = all_samples
            .chunks(2)
            .map(|chunk| ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16)
            .collect();
        all_samples = mono_samples;
    }

    // Resample if needed
    if (resample_ratio - 1.0).abs() > 0.01 {
        // Simple linear resampling
        let orig_len = all_samples.len();
        let new_len = (orig_len as f64 * resample_ratio) as usize;
        let mut resampled = vec![0; new_len];

        for i in 0..new_len {
            let src_idx = i as f64 / resample_ratio;
            let src_idx_floor = src_idx.floor() as usize;
            let src_idx_ceil = (src_idx_floor + 1).min(orig_len - 1);
            let t = src_idx - src_idx_floor as f64;

            resampled[i] = (all_samples[src_idx_floor] as f64 * (1.0 - t)
                + all_samples[src_idx_ceil] as f64 * t) as i16;
        }

        all_samples = resampled;
    }

    // Send PCM data in chunks
    let mut timestamp: u64 = 0;
    let packet_duration = 1000.0 / target_sample_rate as f64 * max_pcm_chunk_size as f64;
    let packet_duration_ms = packet_duration as u64;

    // Stream the audio data
    for chunk in all_samples.chunks(max_pcm_chunk_size) {
        // Check if we should stop
        if token.is_cancelled() {
            break;
        }

        // Create a TrackPacket with PCM data
        let packet = TrackPacket {
            track_id: track_id.to_string(),
            timestamp,
            payload: TrackPayload::PCM(chunk.to_vec()),
        };

        // Send the packet
        if let Err(e) = packet_sender.send(packet) {
            tracing::error!("Failed to send audio packet: {}", e);
            break;
        }

        // Update timestamp for next packet
        timestamp += packet_duration_ms;

        // Sleep for the duration of the packet to simulate real-time streaming
        tokio::time::sleep(Duration::from_millis(packet_duration_ms / 2)).await;
    }

    Ok(())
}
