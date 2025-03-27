use super::*;
use crate::media::processor::AudioPayload;
use anyhow::Result;
use mockall::predicate::*;
use mockall::*;
use std::sync::Arc;
use tokio::sync::broadcast;

// Mock TencentCloud ASR client for testing
mock! {
    pub TencentAsrClient {}

    impl Clone for TencentAsrClient {
        fn clone(&self) -> Self;
    }

    impl AsrClient for TencentAsrClient {
        fn transcribe<'a>(
            &'a self,
            audio_data: &'a [i16],
            sample_rate: u32,
            config: &'a AsrConfig,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>;
    }
}

#[tokio::test]
async fn test_mock_tencent_asr() {
    // Create a mock ASR client
    let mut mock_client = MockTencentAsrClient::new();

    // Set up expectations
    mock_client
        .expect_transcribe()
        .returning(|_audio_data, _sample_rate, _config| {
            Box::pin(async { Ok("测试文本转写".to_string()) })
        });

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

    // Transcribe using the mock client
    let result = mock_client.transcribe(&samples, 16000, &config).await;

    // Verify results
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "测试文本转写");
}

#[tokio::test]
async fn test_asr_processor_with_mock_client() {
    // Create a mock ASR client
    let mut mock_client = MockTencentAsrClient::new();

    // Set up expectations for the mock
    mock_client
        .expect_transcribe()
        .returning(|_audio_data, _sample_rate, _config| {
            Box::pin(async { Ok("测试处理器转写".to_string()) })
        });

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
    let processor =
        AsrProcessor::new(config, Arc::new(mock_client), event_sender).with_buffer_duration(10); // Very small buffer for testing

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

#[tokio::test]
async fn test_audio_buffer_accumulation() {
    // Create a mock ASR client
    let mut mock_client = MockTencentAsrClient::new();

    // Set up expectations for the mock
    mock_client
        .expect_transcribe()
        .returning(|audio_data, _sample_rate, _config| {
            // Return length of audio data in the transcription for verification
            let length = audio_data.len();
            Box::pin(async move { Ok(format!("Audio length: {}", length)) })
        });

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

    // Create processor with specific buffer duration
    let buffer_duration_ms = 500; // 500ms buffer
    let processor = AsrProcessor::new(config, Arc::new(mock_client), event_sender)
        .with_buffer_duration(buffer_duration_ms);

    // Create listener for ASR events
    let event_listener = tokio::spawn(async move {
        match event_receiver.recv().await {
            Ok(event) => Some(event),
            Err(_) => None,
        }
    });

    // Send multiple small audio frames to accumulate in buffer
    let sample_rate = 16000;
    let samples_per_frame = 1600; // 100ms of audio at 16kHz
    let mut frame = AudioFrame {
        track_id: "test".to_string(),
        samples: AudioPayload::PCM(vec![0i16; samples_per_frame]),
        timestamp: 3000,
        sample_rate,
    };

    // Process multiple frames to fill buffer
    for _ in 0..6 {
        let result = processor.process_frame(&mut frame);
        assert!(result.is_ok());
    }

    // Wait for processing to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Check the event
    let event = event_listener.await.unwrap();
    assert!(event.is_some());
    let event = event.unwrap();

    // Expected buffer size is approximately 5-6 frames (since we're right at the threshold)
    let expected_min_size = samples_per_frame * 5;
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
