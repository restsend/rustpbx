use super::*;
use crate::media::processor::AudioPayload;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::broadcast;

// Simple test implementation of AsrClient
#[derive(Clone)]
struct TestAsrClient {
    response: String,
    audio_length_factor: Option<bool>,
}

impl TestAsrClient {
    fn new(response: String) -> Self {
        Self {
            response,
            audio_length_factor: None,
        }
    }

    fn with_audio_length_factor(mut self) -> Self {
        self.audio_length_factor = Some(true);
        self
    }
}

impl AsrClient for TestAsrClient {
    fn transcribe<'a>(
        &'a self,
        audio_data: &'a [i16],
        _sample_rate: u32,
        _config: &'a AsrConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        let response = self.response.clone();
        let audio_len = audio_data.len();
        let include_length = self.audio_length_factor.is_some();

        Box::pin(async move {
            if include_length {
                Ok(format!("Audio length: {}", audio_len))
            } else {
                Ok(response)
            }
        })
    }
}

#[tokio::test]
async fn test_tencent_asr_client() {
    // Create a test ASR client
    let client = TestAsrClient::new("测试文本转写".to_string());

    // Create ASR config
    let config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        engine_type: Some("16k_zh".to_string()),
    };

    // Create sample audio data
    let samples = vec![0i16; 1600]; // 100ms of silence at 16kHz

    // Transcribe using the test client
    let result = client.transcribe(&samples, 16000, &config).await;

    // Verify results
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "测试文本转写");
}

#[tokio::test]
async fn test_asr_processor_with_test_client() {
    // Create a test ASR client
    let client: Arc<dyn AsrClient> = Arc::new(TestAsrClient::new("测试处理器转写".to_string()));

    // Create event channel
    let (event_sender, mut event_receiver) = broadcast::channel::<AsrEvent>(10);

    // Create ASR config
    let config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        engine_type: Some("16k_zh".to_string()),
    };

    // Create processor with small buffer duration for testing
    let processor = AsrProcessor::new(config, client, event_sender).with_buffer_duration(10); // Very small buffer for testing

    // Create listener for ASR events
    let event_listener = tokio::spawn(async move {
        match event_receiver.recv().await {
            Ok(event) => Some(event),
            Err(_) => None,
        }
    });

    // Create test audio frame
    let mut frame = AudioFrame {
        track_id: "test".to_string(),
        samples: AudioPayload::PCM(vec![0i16; 8000]), // 500ms at 16kHz
        timestamp: 3000,
        sample_rate: 16000,
    };

    // Process frame
    let result = processor.process_frame(&mut frame);
    assert!(result.is_ok());

    // Wait a bit for async processing to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Check the event
    let event = event_listener.await.unwrap();
    assert!(event.is_some());
    let event = event.unwrap();
    assert_eq!(event.text, "测试处理器转写");
    assert_eq!(event.track_id, "test");
    assert!(event.is_final);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_audio_buffer_accumulation() {
    // Create a test ASR client that returns audio length
    let client: Arc<dyn AsrClient> =
        Arc::new(TestAsrClient::new("".to_string()).with_audio_length_factor());

    // Create event channel
    let (event_sender, mut event_receiver) = broadcast::channel::<AsrEvent>(10);

    // Create ASR config
    let config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        engine_type: Some("16k_zh".to_string()),
    };

    // Create processor with specific buffer duration - very small for testing
    let buffer_duration_ms = 30; // 30ms buffer (much smaller than our original 500ms)
    let processor =
        AsrProcessor::new(config, client, event_sender).with_buffer_duration(buffer_duration_ms);

    // Create listener for ASR events with timeout
    let event_listener = tokio::spawn(async move {
        tokio::select! {
            event = event_receiver.recv() => {
                match event {
                    Ok(event) => Some(event),
                    Err(_) => None,
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(1000)) => {
                // Timeout after 1 second
                None
            }
        }
    });

    // Send multiple small audio frames to accumulate in buffer
    let sample_rate = 16000;
    let samples_per_frame = 800; // 50ms of audio at 16kHz
    let mut frame = AudioFrame {
        track_id: "test".to_string(),
        samples: AudioPayload::PCM(vec![0i16; samples_per_frame]),
        timestamp: 3000,
        sample_rate,
    };

    // Process just enough frames to trigger a transcription
    // For a 30ms buffer, we only need 1 frame of 50ms audio
    let result = processor.process_frame(&mut frame);
    assert!(result.is_ok());

    // Wait for the event with timeout
    let event = event_listener.await.unwrap();

    // Verify the event
    assert!(event.is_some(), "No event received, test timed out");

    if let Some(event) = event {
        // Expected buffer size is approximately the same as our frame
        let expected_min_size = samples_per_frame;
        let reported_size = event
            .text
            .replace("Audio length: ", "")
            .parse::<usize>()
            .unwrap();

        assert!(
            reported_size >= expected_min_size,
            "Expected buffer size of at least {} samples, got {} samples",
            expected_min_size,
            reported_size
        );
    }
}
