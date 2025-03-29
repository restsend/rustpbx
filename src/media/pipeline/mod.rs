use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::{error, info};

use crate::event::{EventSender, SessionEvent};
use crate::media::processor::AudioFrame;

/// StreamState represents different types of data flowing through the pipeline
#[derive(Debug, Clone)]
pub enum StreamState {
    /// Audio input data
    AudioInput(Vec<i16>, u32), // samples, sample_rate

    /// Transcription result from ASR
    Transcription(String, String), // track_id, text

    /// Transcription segment from ASR (partial result)
    TranscriptionSegment(String, String), // track_id, text

    /// LLM response
    LlmResponse(String), // text

    /// TTS audio output
    TtsAudio(Vec<i16>, u32), // audio samples, sample_rate

    /// Error state
    Error(String), // error message
}

/// Type aliases for StreamState sender and receiver
pub type StreamStateSender = broadcast::Sender<StreamState>;
pub type StreamStateReceiver = broadcast::Receiver<StreamState>;

/// Pipeline trait defines a component in the media processing pipeline
#[async_trait]
pub trait Pipeline: Send + Sync + Debug {
    /// Returns the unique ID of this pipeline
    fn id(&self) -> String;

    /// Starts the pipeline processing
    async fn start(
        &mut self,
        token: CancellationToken,
        state_sender: StreamStateSender,
        state_receiver: StreamStateReceiver,
        event_sender: EventSender,
    ) -> Result<()>;

    /// Stops the pipeline processing
    async fn stop(&self) -> Result<()>;

    /// Processes an audio frame (optional, not all pipelines need to implement this)
    async fn process_frame(&self, _frame: &AudioFrame) -> Result<()> {
        Ok(())
    }

    /// Maps a StreamState to a SessionEvent (if applicable)
    fn map_state_to_event(&self, state: &StreamState, timestamp: u32) -> Option<SessionEvent> {
        match state {
            StreamState::AudioInput(_, _) => None, // Don't map audio input to events
            StreamState::Transcription(track_id, text) => Some(SessionEvent::Transcription(
                track_id.clone(),
                timestamp,
                text.clone(),
            )),
            StreamState::TranscriptionSegment(track_id, text) => Some(
                SessionEvent::TranscriptionSegment(track_id.clone(), timestamp, text.clone()),
            ),
            StreamState::LlmResponse(text) => Some(SessionEvent::LLM(timestamp, text.clone())),
            StreamState::TtsAudio(_, _) => None, // TTS audio is mapped in the TTS pipeline
            StreamState::Error(message) => Some(SessionEvent::Error(timestamp, message.clone())),
        }
    }
}

/// PipelineManager coordinates multiple pipelines
pub struct PipelineManager {
    id: String,
    pipelines: Arc<Mutex<HashMap<String, Box<dyn Pipeline>>>>,
    state_sender: StreamStateSender,
    state_receiver: StreamStateReceiver,
    event_sender: EventSender,
    cancel_token: CancellationToken,
    buffer_size: usize,
}

impl PipelineManager {
    pub fn new(id: String, event_sender: EventSender, cancel_token: CancellationToken) -> Self {
        let buffer_size = 32;
        let (state_sender, state_receiver) = broadcast::channel(buffer_size);

        Self {
            id,
            pipelines: Arc::new(Mutex::new(HashMap::new())),
            state_sender,
            state_receiver,
            event_sender,
            cancel_token,
            buffer_size,
        }
    }

    /// Adds a pipeline to the manager
    pub async fn add_pipeline(&self, mut pipeline: Box<dyn Pipeline>) -> Result<()> {
        let pipeline_id = pipeline.id();
        info!("Adding pipeline {} to manager {}", pipeline_id, self.id);

        // Start the pipeline
        let token = self.cancel_token.child_token();
        let state_sender = self.state_sender.clone();
        let state_receiver = state_sender.subscribe();
        let event_sender = self.event_sender.clone();

        pipeline
            .start(token, state_sender, state_receiver, event_sender)
            .await?;

        // Store the pipeline
        self.pipelines.lock().await.insert(pipeline_id, pipeline);

        Ok(())
    }

    /// Removes a pipeline from the manager
    pub async fn remove_pipeline(&self, pipeline_id: &str) -> Result<()> {
        info!("Removing pipeline {} from manager {}", pipeline_id, self.id);

        if let Some(pipeline) = self.pipelines.lock().await.remove(pipeline_id) {
            pipeline.stop().await?;
        }

        Ok(())
    }

    /// Sends a state update to all pipelines
    pub fn send_state(&self, state: StreamState) -> Result<()> {
        debug!(
            "Sending state update to all pipelines in manager {}",
            self.id
        );
        self.state_sender.send(state)?;
        Ok(())
    }

    /// Processes an audio frame and sends it to the pipeline
    pub async fn process_audio(&self, samples: Vec<i16>, sample_rate: u32) -> Result<()> {
        debug!(
            "Processing audio in manager {}: {} samples at {}Hz",
            self.id,
            samples.len(),
            sample_rate
        );

        // Send the audio samples to the pipeline
        self.send_state(StreamState::AudioInput(samples, sample_rate))?;

        Ok(())
    }

    /// Stops all pipelines
    pub async fn stop(&self) -> Result<()> {
        info!("Stopping all pipelines in manager {}", self.id);

        // Cancel all pipelines
        self.cancel_token.cancel();

        // Stop each pipeline explicitly
        let mut pipelines = self.pipelines.lock().await;
        for (id, pipeline) in pipelines.iter() {
            if let Err(e) = pipeline.stop().await {
                error!("Error stopping pipeline {}: {}", id, e);
            }
        }
        pipelines.clear();

        Ok(())
    }

    /// Returns a new state receiver for a pipeline
    pub fn subscribe(&self) -> StreamStateReceiver {
        self.state_sender.subscribe()
    }
}

pub mod llm;
pub mod standalone_tests;
pub mod synthesis;
pub mod tests;
pub mod transcription;
