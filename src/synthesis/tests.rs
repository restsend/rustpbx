use super::*;
use anyhow::Result;
use dotenv::dotenv;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

// Helper function to get credentials from .env
fn get_tencent_cloud_credentials() -> Option<(String, String, String)> {
    // Load .env file if it exists
    let _ = dotenv();

    // Try to get the credentials from environment variables
    let secret_id = std::env::var("TENCENT_SECRET_ID").ok()?;
    let secret_key = std::env::var("TENCENT_SECRET_KEY").ok()?;
    let appid = std::env::var("TENCENT_APPID").ok()?;

    // Return None if any of the credentials are empty
    if secret_id.is_empty() || secret_key.is_empty() || appid.is_empty() {
        return None;
    }

    Some((secret_id, secret_key, appid))
}

// Simple test implementation of TtsClient
#[derive(Clone)]
struct TestTtsClient {
    response_size: usize,
    capture_voice: Arc<Mutex<Option<String>>>,
    capture_codec: Arc<Mutex<Option<String>>>,
}

impl TestTtsClient {
    fn new(response_size: usize) -> Self {
        Self {
            response_size,
            capture_voice: Arc::new(Mutex::new(None)),
            capture_codec: Arc::new(Mutex::new(None)),
        }
    }

    fn with_capture_params(self) -> Self {
        *self.capture_voice.lock().unwrap() = Some(String::new());
        *self.capture_codec.lock().unwrap() = Some(String::new());
        self
    }

    fn get_captured_voice(&self) -> Option<String> {
        self.capture_voice.lock().unwrap().clone()
    }

    fn get_captured_codec(&self) -> Option<String> {
        self.capture_codec.lock().unwrap().clone()
    }
}

impl TtsClient for TestTtsClient {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        _config: &'a TtsConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        // Generate audio data proportional to text length
        let response_size = self.response_size;
        let text_len = text.len();

        // Capture voice and codec parameters if requested
        if let Some(voice_type) = &_config.voice {
            let voice = self.capture_voice.clone();
            if voice.lock().unwrap().is_some() {
                *voice.lock().unwrap() = Some(voice_type.clone());
            }
        }

        if let Some(codec_type) = &_config.codec {
            let codec = self.capture_codec.clone();
            if codec.lock().unwrap().is_some() {
                *codec.lock().unwrap() = Some(codec_type.clone());
            }
        }

        // Return simulated audio data
        Box::pin(async move { Ok(vec![0u8; response_size * text_len]) })
    }
}

#[tokio::test]
async fn test_tencent_tts_client() {
    // Create a test TTS client
    let client = TestTtsClient::new(100);

    // Create TTS config
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(0),
        speaker: Some(1),
        codec: Some("wav".to_string()),
    };

    // Synthesize using the test client
    let text = "你好世界";
    let result = client.synthesize(text, &config).await;

    // Verify results
    assert!(result.is_ok());
    let audio_data = result.unwrap();
    // Should have a size proportional to the text length
    assert_eq!(audio_data.len(), 100 * text.len());
}

#[tokio::test]
async fn test_tts_processor_with_test_client() {
    // Create a test TTS client
    let client: Arc<dyn TtsClient> = Arc::new(TestTtsClient::new(100));

    // Create event channel
    let (event_sender, mut event_receiver) = broadcast::channel::<TtsEvent>(10);

    // Create TTS config
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(0),
        speaker: Some(1),
        codec: Some("wav".to_string()),
    };

    // Create processor
    let processor = TtsProcessor::new(config, client, event_sender);

    // Create listener for TTS events
    let event_listener = tokio::spawn(async move {
        match event_receiver.recv().await {
            Ok(event) => Some(event),
            Err(_) => None,
        }
    });

    // Synthesize text
    let text = "你好世界";

    let result = processor.synthesize(text).await;
    assert!(result.is_ok());

    // Check the event
    let event = event_listener.await.unwrap();
    assert!(event.is_some());
    let event = event.unwrap();
    assert_eq!(event.text, text);
    // Can't verify the audio data length from event, only from result
    assert_eq!(result.unwrap().len(), 100 * text.len());
}

#[tokio::test]
async fn test_tts_different_voice_types() {
    // Create a test TTS client that captures parameters
    let client = TestTtsClient::new(100).with_capture_params();
    let client_arc: Arc<dyn TtsClient> = Arc::new(client.clone());

    // Create event channel
    let (event_sender, _) = broadcast::channel::<TtsEvent>(10);

    // Create TTS config with voice type 1
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(0),
        speaker: Some(1),
        codec: Some("wav".to_string()),
    };

    // Create processor
    let processor = TtsProcessor::new(config, client_arc, event_sender);

    // Synthesize text
    let text = "你好世界";

    let _ = processor.synthesize(text).await;

    // Get the captured voice parameter
    let voice_param = client.get_captured_voice();
    assert!(voice_param.is_some());
    assert_eq!(voice_param.unwrap(), "1");
}

#[tokio::test]
async fn test_tts_codec_parameter() {
    // Create a test TTS client that captures parameters
    let client = TestTtsClient::new(100).with_capture_params();
    let client_arc: Arc<dyn TtsClient> = Arc::new(client.clone());

    // Create event channel
    let (event_sender, _) = broadcast::channel::<TtsEvent>(10);

    // Create TTS config with wav codec
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some("test_appid".to_string()),
        secret_id: Some("test_secret_id".to_string()),
        secret_key: Some("test_secret_key".to_string()),
        volume: Some(0),
        speaker: Some(1),
        codec: Some("wav".to_string()),
    };

    // Create processor
    let processor = TtsProcessor::new(config, client_arc, event_sender);

    // Synthesize text
    let text = "你好世界";

    let _ = processor.synthesize(text).await;

    // Get the captured codec parameter
    let codec_param = client.get_captured_codec();
    assert!(codec_param.is_some());
    assert_eq!(codec_param.unwrap(), "wav");
}

#[tokio::test]
async fn test_real_tencent_tts_client() {
    // Load credentials from .env
    let credentials = get_tencent_cloud_credentials();

    // Skip test if credentials aren't available
    if credentials.is_none() {
        println!("Skipping real TencentCloud TTS client test: Missing credentials in .env file");
        return;
    }

    let (secret_id, secret_key, appid) = credentials.unwrap();

    // Create real TTS client
    let client = TencentCloudTtsClient::new();

    // Create TTS config with real credentials
    let config = TtsConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(1.0),
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        volume: Some(5),
        speaker: Some(1),
        codec: Some("pcm".to_string()),
    };

    // Synthesize using the real client (short text for quick test)
    let text = "集成测试";
    let result = client.synthesize(text, &config).await;

    // Verify results
    assert!(result.is_ok());
    let audio_data = result.unwrap();
    assert!(!audio_data.is_empty(), "Audio data should not be empty");
}
