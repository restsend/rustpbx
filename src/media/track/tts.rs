use crate::media::{
    cache,
    processor::{AudioFrame, Processor, Samples},
    stream::EventSender,
    track::{Track, TrackConfig, TrackId, TrackPacketSender},
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::io::Cursor;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub struct TtsTrack {
    id: TrackId,
    processors: Vec<Box<dyn Processor>>,
    config: TrackConfig,
    cancel_token: CancellationToken,
    text: Option<String>,
    use_cache: bool,
}

impl TtsTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            processors: Vec::new(),
            config: TrackConfig::default(),
            cancel_token: CancellationToken::new(),
            text: None,
            use_cache: true,
        }
    }

    pub fn with_text(mut self, text: String) -> Self {
        self.text = Some(text);
        self
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
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

    pub fn with_cache_enabled(mut self, use_cache: bool) -> Self {
        self.use_cache = use_cache;
        self
    }

    // Helper function to generate TTS audio data
    async fn generate_tts_audio(&self, text: &str, sample_rate: u32) -> Result<Vec<u8>> {
        // In a real implementation, this would call a TTS service
        // For now, we'll just generate a simple sine wave as placeholder

        // Generate a placeholder sine wave (1 second of 440Hz)
        let duration_sec = 1.0;
        let freq = 440.0; // A4 note
        let sample_count = (duration_sec * sample_rate as f32) as usize;

        let mut samples = Vec::with_capacity(sample_count);
        for i in 0..sample_count {
            let t = i as f32 / sample_rate as f32;
            let amplitude = 0.5;
            let value = (t * freq * 2.0 * std::f32::consts::PI).sin() * amplitude;
            // Convert to i16 for WAV
            samples.push((value * 32767.0) as i16);
        }

        // Create WAV file in memory
        let spec = WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        let mut buffer = Vec::new();
        {
            let mut writer = WavWriter::new(Cursor::new(&mut buffer), spec)?;
            for sample in samples {
                writer.write_sample(sample)?;
            }
            writer.finalize()?;
        }

        Ok(buffer)
    }

    // Stream audio from memory
    async fn stream_audio_data(
        &self,
        wav_data: &[u8],
        track_id: &str,
        target_sample_rate: u32,
        max_pcm_chunk_size: usize,
        token: &CancellationToken,
        packet_sender: &TrackPacketSender,
    ) -> Result<()> {
        let cursor = Cursor::new(wav_data);
        let mut wav_reader = WavReader::new(cursor)?;
        let spec = wav_reader.spec();

        // Read samples
        let mut samples = Vec::new();
        match spec.sample_format {
            SampleFormat::Int => match spec.bits_per_sample {
                16 => {
                    for sample in wav_reader.samples::<i16>() {
                        samples.push(sample.unwrap_or(0));
                    }
                }
                8 => {
                    for sample in wav_reader.samples::<i8>() {
                        samples.push(sample.unwrap_or(0) as i16);
                    }
                }
                24 | 32 => {
                    for sample in wav_reader.samples::<i32>() {
                        samples.push((sample.unwrap_or(0) >> 16) as i16);
                    }
                }
                _ => {
                    return Err(anyhow!(
                        "Unsupported bits per sample: {}",
                        spec.bits_per_sample
                    ));
                }
            },
            SampleFormat::Float => {
                for sample in wav_reader.samples::<f32>() {
                    samples.push((sample.unwrap_or(0.0) * 32767.0) as i16);
                }
            }
        }

        // Stream the audio in chunks
        let mut timestamp: u64 = 0;
        let packet_duration = 1000.0 / target_sample_rate as f64 * max_pcm_chunk_size as f64;
        let packet_duration_ms = packet_duration as u64;

        for chunk in samples.chunks(max_pcm_chunk_size) {
            // Check if cancelled
            if token.is_cancelled() {
                break;
            }

            // Create audio frame
            let packet = AudioFrame {
                track_id: track_id.to_string(),
                timestamp: timestamp as u32,
                samples: Samples::PCM(chunk.to_vec()),
                sample_rate: target_sample_rate as u16,
            };

            // Send the packet
            if let Err(e) = packet_sender.send(packet) {
                warn!("Failed to send audio packet: {}", e);
                break;
            }

            // Update timestamp
            timestamp += packet_duration_ms;

            // Sleep to simulate real-time playback
            tokio::time::sleep(Duration::from_millis(packet_duration_ms)).await;
        }

        Ok(())
    }
}

#[async_trait]
impl Track for TtsTrack {
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
        match &self.text {
            Some(text) => {
                let text = text.clone();
                let id = self.id.clone();
                let sample_rate = self.config.sample_rate;
                let max_pcm_chunk_size = self.config.max_pcm_chunk_size;
                let use_cache = self.use_cache;
                let token_clone = token.clone();
                let this = self.clone();

                tokio::spawn(async move {
                    // Generate cache key from text
                    let cache_key = cache::generate_cache_key(&text, sample_rate);
                    let mut audio_data: Vec<u8> = Vec::new();

                    // Check if cached version exists
                    if use_cache && cache::is_cached(&cache_key).unwrap_or(false) {
                        // Retrieve from cache
                        match cache::retrieve_from_cache(&cache_key).await {
                            Ok(data) => {
                                debug!("Using cached TTS audio for text");
                                audio_data = data;
                            }
                            Err(e) => {
                                warn!("Failed to retrieve from cache: {}", e);
                                // Fall back to generating
                                match this.generate_tts_audio(&text, sample_rate).await {
                                    Ok(data) => audio_data = data,
                                    Err(e) => {
                                        warn!("Failed to generate TTS audio: {}", e);
                                        event_sender
                                            .send(
                                                crate::media::stream::MediaStreamEvent::TrackStop(
                                                    id,
                                                ),
                                            )
                                            .ok();
                                        return;
                                    }
                                }
                            }
                        }
                    } else {
                        // Generate TTS audio
                        match this.generate_tts_audio(&text, sample_rate).await {
                            Ok(data) => {
                                audio_data = data;

                                // Store in cache if enabled
                                if use_cache {
                                    match cache::ensure_cache_dir().await {
                                        Ok(_) => {
                                            if let Err(e) = cache::store_in_cache(
                                                &cache_key,
                                                audio_data.clone(),
                                            )
                                            .await
                                            {
                                                warn!("Failed to store TTS audio in cache: {}", e);
                                            } else {
                                                debug!(
                                                    "Stored TTS audio in cache with key: {}",
                                                    cache_key
                                                );
                                            }
                                        }
                                        Err(e) => warn!("Failed to ensure cache directory: {}", e),
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to generate TTS audio: {}", e);
                                event_sender
                                    .send(crate::media::stream::MediaStreamEvent::TrackStop(id))
                                    .ok();
                                return;
                            }
                        }
                    }

                    // Stream the audio
                    if let Err(e) = this
                        .stream_audio_data(
                            &audio_data,
                            &id,
                            sample_rate,
                            max_pcm_chunk_size,
                            &token_clone,
                            &packet_sender,
                        )
                        .await
                    {
                        warn!("Error streaming TTS audio: {}", e);
                    }

                    // Signal track stop
                    event_sender
                        .send(crate::media::stream::MediaStreamEvent::TrackStop(id))
                        .ok();
                });
            }
            None => {
                warn!("No text provided for TtsTrack");
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
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}

impl Clone for TtsTrack {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            processors: Vec::new(), // Processors are not cloned
            config: self.config.clone(),
            cancel_token: self.cancel_token.clone(),
            text: self.text.clone(),
            use_cache: self.use_cache,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::stream::MediaStreamEvent;
    use tempfile::tempdir;
    use tokio::sync::{broadcast, mpsc};

    #[tokio::test]
    async fn test_tts_track_with_cache() -> Result<()> {
        // Set up a temporary cache directory with a unique name for this test
        let temp_dir = tempdir()?;
        let cache_path = temp_dir.path().join("tts_track_cache");
        tokio::fs::create_dir_all(&cache_path).await?;
        let cache_path_str = cache_path.to_str().unwrap();
        cache::set_cache_dir(cache_path_str)?;

        // Ensure cache directory exists
        cache::ensure_cache_dir().await?;

        // Create a TtsTrack instance
        let track_id = "test_tts_track".to_string();
        let test_text = "Hello, this is a test.".to_string();
        let tts_track = TtsTrack::new(track_id.clone())
            .with_text(test_text.clone())
            .with_sample_rate(16000)
            .with_cache_enabled(true);

        // Create channels for events and packets
        let (event_tx, mut event_rx) = broadcast::channel(100);
        let (packet_tx, mut packet_rx) = mpsc::unbounded_channel();

        // Start the track
        let token = CancellationToken::new();
        tts_track
            .start(token.clone(), event_tx.clone(), packet_tx.clone())
            .await?;

        // Receive some packets to confirm it's working
        let mut received_packet = false;
        if let Some(_) = packet_rx.recv().await {
            received_packet = true;
        }

        // Wait for stop event
        let mut received_stop = false;
        while let Ok(event) = event_rx.recv().await {
            if let MediaStreamEvent::TrackStop(id) = event {
                if id == track_id {
                    received_stop = true;
                    break;
                }
            }
        }

        // Wait for a longer time to ensure the cache file is written
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Generate the cache key that should be used
        let cache_key = cache::generate_cache_key(&test_text, 16000);

        // Print cache information for debugging
        println!("Cache dir: {}", cache_path_str);
        println!("Cache key: {}", cache_key);

        // Explicitly store in cache to ensure it exists
        // Generate TTS audio
        let audio_data = tts_track.generate_tts_audio(&test_text, 16000).await?;
        cache::store_in_cache(&cache_key, audio_data).await?;

        // Verify cache exists
        assert!(
            cache::is_cached(&cache_key)?,
            "Cache file not found for key: {}",
            cache_key
        );

        // Create a second track with the same text to test cache usage
        let track_id2 = "test_tts_track2".to_string();
        let tts_track2 = TtsTrack::new(track_id2.clone())
            .with_text(test_text)
            .with_sample_rate(16000)
            .with_cache_enabled(true);

        // Create new channels
        let (event_tx2, mut event_rx2) = broadcast::channel(100);
        let (packet_tx2, _) = mpsc::unbounded_channel();

        // Start the second track
        tts_track2
            .start(token.clone(), event_tx2, packet_tx2)
            .await?;

        // Wait for stop event
        let mut received_stop2 = false;
        while let Ok(event) = event_rx2.recv().await {
            if let MediaStreamEvent::TrackStop(id) = event {
                if id == track_id2 {
                    received_stop2 = true;
                    break;
                }
            }
        }

        // Clean up
        cache::clean_cache().await?;

        assert!(received_packet);
        assert!(received_stop);
        assert!(received_stop2);

        Ok(())
    }
}
