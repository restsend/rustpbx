use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::{sync::broadcast, time::Duration};
use tokio_util::sync::CancellationToken;

use crate::event::EventSender;
use crate::media::pipeline::{
    Pipeline, PipelineManager, StreamState, StreamStateReceiver, StreamStateSender,
};
use crate::media::processor::AudioFrame;

// A simple mock pipeline for testing
#[derive(Debug)]
struct MockPipeline {
    id: String,
    token: Option<CancellationToken>,
}

impl MockPipeline {
    fn new(id: String) -> Self {
        Self { id, token: None }
    }
}

#[async_trait]
impl Pipeline for MockPipeline {
    fn id(&self) -> String {
        self.id.clone()
    }

    async fn start(
        &mut self,
        token: CancellationToken,
        state_sender: StreamStateSender,
        state_receiver: StreamStateReceiver,
        _event_sender: EventSender,
    ) -> Result<()> {
        println!("Starting mock pipeline {}", self.id);
        self.token = Some(token.clone());

        let id = self.id.clone();
        let state_sender_clone = state_sender.clone();
        let mut receiver = state_receiver;

        // Start processing in a separate task
        tokio::spawn(async move {
            println!("Mock pipeline {} started", id);

            // Process incoming stream states
            while let Ok(state) = receiver.recv().await {
                if token.is_cancelled() {
                    break;
                }

                match state {
                    StreamState::AudioInput(samples, rate) => {
                        println!(
                            "MockPipeline {}: Processing {} samples at rate {}",
                            id,
                            samples.len(),
                            rate
                        );

                        // Echo back transcription
                        if let Err(e) = state_sender_clone.send(StreamState::Transcription(
                            id.clone(),
                            format!("Mock result from {}", id),
                        )) {
                            println!("Failed to send transcription: {}", e);
                        }
                    }
                    StreamState::LlmResponse(text) => {
                        println!("MockPipeline {}: Processing LLM result: {}", id, text);

                        // Echo back a synthesis result
                        let response_samples = vec![0i16; 1000];
                        if let Err(e) =
                            state_sender_clone.send(StreamState::TtsAudio(response_samples, 16000))
                        {
                            println!("Failed to send TTS audio: {}", e);
                        }
                    }
                    _ => {
                        // Ignore other states
                    }
                }
            }

            println!("Mock pipeline {} stopped", id);
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = &self.token {
            token.cancel();
        }
        Ok(())
    }

    async fn process_frame(&self, _frame: &AudioFrame) -> Result<()> {
        // Not used in this test
        Ok(())
    }
}

#[tokio::test]
async fn test_standalone_pipeline_manager() -> Result<()> {
    // Create a session event sender for the pipeline manager
    let (session_sender, _) = broadcast::channel(32);
    let cancel_token = CancellationToken::new();

    // Create the pipeline manager
    let pipeline_manager = Arc::new(PipelineManager::new(
        "test-pipeline".to_string(),
        session_sender,
        cancel_token.clone(),
    ));

    // Create and add mock pipelines
    let mock_pipeline1 = MockPipeline::new("mock-pipeline-1".to_string());
    let mock_pipeline2 = MockPipeline::new("mock-pipeline-2".to_string());

    pipeline_manager
        .add_pipeline(Box::new(mock_pipeline1))
        .await?;
    pipeline_manager
        .add_pipeline(Box::new(mock_pipeline2))
        .await?;

    // Subscribe to the pipeline state
    let mut state_receiver = pipeline_manager.subscribe();

    // Send an audio input via the pipeline manager's send_state method
    let test_samples = vec![0i16; 1000];
    pipeline_manager.send_state(StreamState::AudioInput(test_samples, 16000))?;

    // Check the events coming from the pipeline manager
    let mut received_transcription = false;
    let mut received_tts_audio = false;

    // Set a timeout for the test
    let timeout = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(timeout);

    // Process events with a timeout
    loop {
        tokio::select! {
            _ = &mut timeout => {
                break;
            }
            Ok(state) = state_receiver.recv() => {
                match state {
                    StreamState::Transcription(id, text) => {
                        println!("Received transcription from {}: {}", id, text);
                        assert!(text.starts_with("Mock result from"));
                        received_transcription = true;

                        // Send LLM result to test synthesis
                        pipeline_manager.send_state(StreamState::LlmResponse("Test LLM response".to_string()))?;
                    }
                    StreamState::TtsAudio(samples, rate) => {
                        println!("Received TTS audio: {} samples at {}Hz", samples.len(), rate);
                        assert_eq!(samples.len(), 1000);
                        received_tts_audio = true;

                        // If we received both events, we can break
                        if received_transcription {
                            break;
                        }
                    }
                    _ => { /* Ignore other states */ }
                }
            }
        }
    }

    // Clean up
    pipeline_manager.stop().await?;

    assert!(
        received_transcription,
        "Should have received a transcription result"
    );
    assert!(received_tts_audio, "Should have received TTS audio");

    Ok(())
}

#[tokio::test]
async fn test_standalone_stream_state() -> Result<()> {
    // Test broadcast capability of StreamState
    let (sender, mut receiver1) = broadcast::channel::<StreamState>(32);
    let mut receiver2 = sender.subscribe();

    // Send a test audio input
    let test_samples = vec![0i16; 1000];
    sender.send(StreamState::AudioInput(test_samples.clone(), 16000))?;

    // Check that both receivers got the message
    if let Ok(StreamState::AudioInput(samples, rate)) = receiver1.try_recv() {
        assert_eq!(samples.len(), 1000);
        assert_eq!(rate, 16000);
    } else {
        panic!("Receiver 1 didn't get the message");
    }

    if let Ok(StreamState::AudioInput(samples, rate)) = receiver2.try_recv() {
        assert_eq!(samples.len(), 1000);
        assert_eq!(rate, 16000);
    } else {
        panic!("Receiver 2 didn't get the message");
    }

    Ok(())
}
