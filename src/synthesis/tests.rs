use super::*;
use anyhow::Result;
use mockall::predicate::*;
use mockall::*;
use std::sync::Arc;

// Mock TencentCloud TTS client for testing
mock! {
    pub TencentTtsClient {}

    impl Clone for TencentTtsClient {
        fn clone(&self) -> Self;
    }

    impl TtsClient for TencentTtsClient {
        fn synthesize<'a>(
            &'a self,
            text: &'a str,
            config: &'a TtsConfig,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>>;
    }
}

#[tokio::test]
async fn test_mock_tencent_tts() {
    // Create a mock TTS client
    let mut mock_client = MockTencentTtsClient::new();

    // Set up expectations
    mock_client.expect_synthesize().returning(|text, _config| {
        // Return dummy audio data with length based on text length
        let audio_length = text.len() * 100;
        Box::pin(async move { Ok(vec![1u8; audio_length]) })
    });

    // Create TTS config
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(5),
        speaker: Some(1),
        codec: Some("pcm".to_string()),
    };

    // Synthesize using the mock client
    let text = "测试文本合成";
    let result = mock_client.synthesize(text, &config).await;

    // Verify results
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), text.len() * 100);
}

#[tokio::test]
async fn test_tts_processor_with_mock_client() {
    // Create a mock TTS client
    let mut mock_client = MockTencentTtsClient::new();

    // Set up expectations for the mock
    mock_client.expect_synthesize().returning(|text, _config| {
        // Return audio length proportional to text
        let length = text.len() * 200;
        Box::pin(async move { Ok(vec![2u8; length]) })
    });

    // Create TTS config
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(5),
        speaker: Some(1),
        codec: Some("pcm".to_string()),
    };

    // Create event channel and processor
    let (event_sender, mut event_receiver) = broadcast::channel::<TtsEvent>(10);
    let processor = TtsProcessor::new(config, Arc::new(mock_client), event_sender);

    // Create event listener
    let event_listener = tokio::spawn(async move {
        match event_receiver.recv().await {
            Ok(event) => Some(event),
            Err(_) => None,
        }
    });

    // Synthesize text
    let text = "语音合成测试";
    let result = processor.synthesize(text).await;

    // Verify synthesis result
    assert!(result.is_ok());
    let audio_data = result.unwrap();
    assert_eq!(audio_data.len(), text.len() * 200);
    assert_eq!(audio_data[0], 2u8); // Check content matches our mock

    // Verify event was sent
    let event = event_listener.await.unwrap();
    assert!(event.is_some());
    let event = event.unwrap();
    assert_eq!(event.text, text);
}

#[tokio::test]
async fn test_tts_different_voice_types() {
    // Create a mock TTS client
    let mut mock_client = MockTencentTtsClient::new();

    // Capture voice parameter
    let voice_type_capture = Arc::new(std::sync::Mutex::new(String::new()));
    let voice_type_capture_clone = voice_type_capture.clone();

    // Set up expectations
    mock_client
        .expect_synthesize()
        .returning(move |_text, config| {
            // Capture the voice type for verification
            let voice = config.voice.clone().unwrap_or_default();
            *voice_type_capture_clone.lock().unwrap() = voice;

            // Return dummy audio data
            Box::pin(async { Ok(vec![3u8; 1000]) })
        });

    // Create base TTS config
    let base_config = TtsConfig {
        url: "".to_string(),
        voice: None, // We'll set this in each test
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(5),
        speaker: Some(1),
        codec: Some("pcm".to_string()),
    };

    // Test Chinese voice
    let mut config = base_config.clone();
    config.voice = Some("1".to_string()); // Chinese voice

    let result = mock_client.synthesize("Chinese text", &config).await;
    assert!(result.is_ok());
    assert_eq!(*voice_type_capture.lock().unwrap(), "1");

    // Test English voice
    let mut config = base_config.clone();
    config.voice = Some("101".to_string()); // English voice

    let result = mock_client.synthesize("English text", &config).await;
    assert!(result.is_ok());
    assert_eq!(*voice_type_capture.lock().unwrap(), "101");
}

#[tokio::test]
async fn test_tts_codec_parameter() {
    // Create a mock TTS client
    let mut mock_client = MockTencentTtsClient::new();

    // Capture codec parameter
    let codec_capture = Arc::new(std::sync::Mutex::new(String::new()));
    let codec_capture_clone = codec_capture.clone();

    // Set up expectations
    mock_client
        .expect_synthesize()
        .returning(move |_text, config| {
            // Capture the codec for verification
            let codec = config.codec.clone().unwrap_or_default();
            *codec_capture_clone.lock().unwrap() = codec;

            // Return dummy audio data
            Box::pin(async { Ok(vec![4u8; 1000]) })
        });

    // Create base TTS config
    let base_config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(5),
        speaker: Some(1),
        codec: None, // We'll set this in each test
    };

    // Test PCM codec
    let mut config = base_config.clone();
    config.codec = Some("pcm".to_string());

    let result = mock_client.synthesize("PCM codec test", &config).await;
    assert!(result.is_ok());
    assert_eq!(*codec_capture.lock().unwrap(), "pcm");

    // Test MP3 codec
    let mut config = base_config.clone();
    config.codec = Some("mp3".to_string());

    let result = mock_client.synthesize("MP3 codec test", &config).await;
    assert!(result.is_ok());
    assert_eq!(*codec_capture.lock().unwrap(), "mp3");
}
