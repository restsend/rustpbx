use crate::media::codecs::resample;
use crate::media::processor::AudioPayload;
use crate::media::{
    processor::{AudioFrame, Processor},
    stream::EventSender,
    track::{Track, TrackConfig, TrackId, TrackPacketSender},
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hound::WavReader;
use std::fs::File;
use std::io::BufReader;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;
pub struct FileTrack {
    id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    processors: Vec<Box<dyn Processor>>,
    path: Option<String>,
}

impl FileTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            config: TrackConfig::default(),
            cancel_token: CancellationToken::new(),
            processors: Vec::new(),
            path: None,
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
        match &self.path {
            Some(path) => {
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
                    event_sender
                        .send(crate::media::stream::MediaStreamEvent::TrackStop(id))
                        .ok();
                });
            }
            None => {
                tracing::error!("No path provided for FileTrack");
                event_sender
                    .send(crate::media::stream::MediaStreamEvent::TrackStop(
                        self.id.clone(),
                    ))
                    .ok();
            }
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Nothing to do here as the file streaming task will stop when the token is cancelled
        self.cancel_token.cancel();
        Ok(())
    }

    // Do nothing as we are not sending packets
    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
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

    // If stereo, convert to mono by averaging channels
    if spec.channels == 2 {
        let mono_samples = all_samples
            .chunks(2)
            .map(|chunk| ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16)
            .collect();
        all_samples = mono_samples;
    }

    // Resample if needed
    if spec.sample_rate != target_sample_rate {
        all_samples = resample::resample_mono(&all_samples, spec.sample_rate, target_sample_rate);
    }

    // Send PCM data in chunks
    let mut timestamp: u64 = 0;
    let packet_duration = 1000.0 / target_sample_rate as f64 * max_pcm_chunk_size as f64;
    let packet_duration_ms = packet_duration as u64;

    info!(
        "Streaming WAV file with {} samples, packet_duration_ms: {} target_sample_rate: {} spec.sample_rate: {}",
        all_samples.len(),
        packet_duration_ms,
        target_sample_rate,
        spec.sample_rate
    );
    // Stream the audio data
    for chunk in all_samples.chunks(max_pcm_chunk_size) {
        // Check if we should stop
        if token.is_cancelled() {
            break;
        }

        // Create a TrackPacket with PCM data
        let packet = AudioFrame {
            track_id: track_id.to_string(),
            timestamp: timestamp as u32,
            samples: AudioPayload::PCM(chunk.to_vec()),
            sample_rate: target_sample_rate as u16,
        };
        // Send the packet
        if let Err(e) = packet_sender.send(packet) {
            tracing::error!("Failed to send audio packet: {}", e);
            break;
        }

        // Update timestamp for next packet
        timestamp += packet_duration_ms;
        // Sleep for the duration of the packet to simulate real-time streaming
        tokio::time::sleep(Duration::from_millis(packet_duration_ms)).await;
    }

    Ok(())
}
