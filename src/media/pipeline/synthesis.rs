use super::{Pipeline, StreamState, StreamStateReceiver, StreamStateSender};
use crate::event::{EventSender, SessionEvent};
use crate::synthesis::{SynthesisClient, SynthesisConfig};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Pipeline processor that handles text-to-speech synthesis
#[derive(Debug)]
pub struct SynthesisPipeline {
    id: String,
    client: Arc<dyn SynthesisClient>,
    config: SynthesisConfig,
    token: Option<CancellationToken>,
}

impl SynthesisPipeline {
    pub fn new(id: String, client: Arc<dyn SynthesisClient>, config: SynthesisConfig) -> Self {
        Self {
            id,
            client,
            config,
            token: None,
        }
    }

    /// Create a new SynthesisPipeline from a JSON configuration
    pub fn from_config(id: String, config_json: Option<serde_json::Value>) -> Self {
        // Default configuration
        let mut config = SynthesisConfig {
            url: "http://localhost:5002/api/tts".to_string(),
            voice: None,
            rate: None,
            appid: None,
            secret_id: None,
            secret_key: None,
            volume: None,
            speaker: None,
            codec: None,
        };

        // Update config from JSON if provided
        if let Some(json) = config_json {
            if let Some(voice) = json.get("voice").and_then(|v| v.as_str()) {
                config.voice = Some(voice.to_string());
            }

            if let Some(rate) = json.get("rate").and_then(|v| v.as_f64()) {
                config.rate = Some(rate as f32);
            }

            if let Some(url) = json.get("url").and_then(|v| v.as_str()) {
                config.url = url.to_string();
            }

            // Parse additional fields if present
            if let Some(appid) = json.get("appid").and_then(|v| v.as_str()) {
                config.appid = Some(appid.to_string());
            }

            if let Some(secret_id) = json.get("secret_id").and_then(|v| v.as_str()) {
                config.secret_id = Some(secret_id.to_string());
            }

            if let Some(secret_key) = json.get("secret_key").and_then(|v| v.as_str()) {
                config.secret_key = Some(secret_key.to_string());
            }

            if let Some(volume) = json.get("volume").and_then(|v| v.as_i64()) {
                config.volume = Some(volume as i32);
            }

            if let Some(speaker) = json.get("speaker").and_then(|v| v.as_i64()) {
                config.speaker = Some(speaker as i32);
            }

            if let Some(codec) = json.get("codec").and_then(|v| v.as_str()) {
                config.codec = Some(codec.to_string());
            }
        }

        // Create a simple mock synthesis client
        #[derive(Debug)]
        struct MockSynthesisClient;

        #[async_trait]
        impl SynthesisClient for MockSynthesisClient {
            async fn synthesize(&self, text: &str, _config: &SynthesisConfig) -> Result<Vec<u8>> {
                // This is a mock that just returns some dummy audio data
                // In reality, this would call a TTS service

                // Generate a simple sine wave as fake audio data
                let sample_rate = 16000u32;
                let duration_secs = 2.0;
                let num_samples = (sample_rate as f32 * duration_secs) as usize;

                let mut samples = Vec::with_capacity(num_samples);

                // Generate a sine wave at 440 Hz
                for i in 0..num_samples {
                    let t = i as f32 / sample_rate as f32;
                    let value = (2.0 * std::f32::consts::PI * 440.0 * t).sin();

                    // Convert to i16 and then to bytes
                    let sample = (value * 32767.0) as i16;
                    samples.push(sample);
                }

                // Convert samples to bytes
                let mut audio_bytes = Vec::with_capacity(samples.len() * 2);
                for sample in samples {
                    let bytes = sample.to_le_bytes();
                    audio_bytes.push(bytes[0]);
                    audio_bytes.push(bytes[1]);
                }

                // Log that we generated fake audio for this text
                info!("Generated mock audio for text: {}", text);

                Ok(audio_bytes)
            }
        }

        let client: Arc<dyn SynthesisClient> = Arc::new(MockSynthesisClient);

        Self {
            id,
            client,
            config,
            token: None,
        }
    }
}

#[async_trait]
impl Pipeline for SynthesisPipeline {
    fn id(&self) -> String {
        self.id.clone()
    }

    async fn start(
        &mut self,
        token: CancellationToken,
        state_sender: StreamStateSender,
        state_receiver: StreamStateReceiver,
        event_sender: EventSender,
    ) -> Result<()> {
        info!("Starting synthesis pipeline {}", self.id);
        self.token = Some(token.clone());

        let client = Arc::clone(&self.client);
        let config = self.config.clone();
        let id = self.id.clone();

        let state_sender_clone = state_sender.clone();
        let event_sender_clone = event_sender.clone();
        let mut receiver = state_receiver;

        // Start the pipeline processing in a separate task
        tokio::spawn(async move {
            info!("Synthesis pipeline {} started", id);

            // Process incoming stream states
            while let Ok(state) = receiver.recv().await {
                if token.is_cancelled() {
                    break;
                }

                match state {
                    StreamState::LlmResponse(text) => {
                        info!("Synthesis pipeline received LLM response: {}", text);

                        // Process through TTS
                        match client.synthesize(&text, &config).await {
                            Ok(audio_data) => {
                                // Convert the audio bytes to samples
                                match convert_to_samples(audio_data) {
                                    Ok((samples, sample_rate)) => {
                                        info!(
                                            "Generated {} TTS samples at {}Hz",
                                            samples.len(),
                                            sample_rate
                                        );

                                        // Send the TTS audio to the pipeline
                                        if let Err(e) = state_sender_clone.send(
                                            StreamState::TtsAudio(samples.clone(), sample_rate),
                                        ) {
                                            error!("Failed to send TTS audio to pipeline: {}", e);
                                        }

                                        // Also send a TTS event directly
                                        let timestamp = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_secs()
                                            as u32;

                                        if let Err(e) = event_sender_clone
                                            .send(SessionEvent::TTS(timestamp, text.clone()))
                                        {
                                            error!("Failed to send TTS event: {}", e);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to convert TTS audio: {}", e);
                                        if let Err(e) = state_sender_clone.send(StreamState::Error(
                                            format!("TTS conversion error: {}", e),
                                        )) {
                                            error!("Failed to send error to pipeline: {}", e);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!("TTS synthesis failed: {}", e);
                                if let Err(e) = state_sender_clone
                                    .send(StreamState::Error(format!("TTS error: {}", e)))
                                {
                                    error!("Failed to send error to pipeline: {}", e);
                                }
                            }
                        }
                    }
                    _ => {
                        // Ignore other state types
                    }
                }
            }

            info!("Synthesis pipeline {} stopped", id);
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = &self.token {
            token.cancel();
        }
        Ok(())
    }

    fn map_state_to_event(&self, state: &StreamState, timestamp: u32) -> Option<SessionEvent> {
        match state {
            StreamState::TtsAudio(samples, _) => {
                // For TTS audio, we might want to send the actual audio samples in an event
                let track_id = self.id.clone();
                Some(SessionEvent::Audio(track_id, timestamp, samples.clone()))
            }
            _ => super::Pipeline::map_state_to_event(self, state, timestamp),
        }
    }
}

// Helper function to convert audio bytes to PCM samples
fn convert_to_samples(audio_data: Vec<u8>) -> Result<(Vec<i16>, u32)> {
    // This is a simplified implementation; in a real application,
    // you would need to parse the audio format properly

    // For now, we'll assume the audio is already in PCM format
    // with 16-bit samples at 16kHz
    let sample_rate = 16000;

    // Convert bytes to i16 samples
    let mut samples = Vec::with_capacity(audio_data.len() / 2);
    let mut i = 0;
    while i < audio_data.len() - 1 {
        let sample = i16::from_le_bytes([audio_data[i], audio_data[i + 1]]);
        samples.push(sample);
        i += 2;
    }

    Ok((samples, sample_rate))
}
