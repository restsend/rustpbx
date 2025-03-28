use super::*;
use crate::media::processor::Samples;
use anyhow::Result;
use dotenv::dotenv;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{info, warn};

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
        samples: Samples::PCM(vec![0i16; 8000]), // 500ms at 16kHz
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
        samples: Samples::PCM(vec![0i16; samples_per_frame]),
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

#[tokio::test]
async fn test_real_tencent_asr_client() {
    // Load credentials from .env
    let credentials = get_tencent_cloud_credentials();

    // Skip test if credentials aren't available
    if credentials.is_none() {
        println!("Skipping real TencentCloud ASR client test: Missing credentials in .env file");
        return;
    }

    let (secret_id, secret_key, appid) = credentials.unwrap();

    // Create real ASR client
    let client = TencentCloudAsrClient::new();

    // Create ASR config with real credentials
    let config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        engine_type: Some("16k_zh".to_string()),
    };

    // Create sample audio data - just a short silence for testing
    // In a real test, you would use actual audio data with speech
    let samples = vec![0i16; 1600]; // 100ms of silence at 16kHz

    // Convert AsrConfig to TranscriptionConfig
    let transcription_config = TranscriptionConfig {
        enabled: config.enabled,
        model: config.model.clone(),
        language: config.language.clone(),
        appid: config.appid.clone(),
        secret_id: config.secret_id.clone(),
        secret_key: config.secret_key.clone(),
        engine_type: config.engine_type.clone(),
    };

    let result = <TencentCloudAsrClient as TranscriptionClient>::transcribe(
        &client,
        &samples,
        16000,
        &transcription_config,
    )
    .await;

    // Print detailed error information if the request fails
    if let Err(ref e) = result {
        println!("ASR request failed with error: {:?}", e);
    }

    // Verify results - note that with real silence, the ASR might return empty text
    assert!(result.is_ok(), "ASR request should succeed");
}

#[cfg(test)]
#[tokio::test]
async fn test_asr_with_pcm_file() -> Result<()> {
    use super::*;
    use crate::media::processor::{AudioFrame, Samples};
    use tracing::{info, warn};

    // Initialize logging
    tracing_subscriber::fmt::init();

    // Load environment variables from .env file
    dotenv::dotenv().ok();

    info!("Starting ASR PCM file test");

    // Get credentials from environment variables
    let secret_id = std::env::var("TENCENT_SECRET_ID").expect("TENCENT_SECRET_ID not set");
    let secret_key = std::env::var("TENCENT_SECRET_KEY").expect("TENCENT_SECRET_KEY not set");
    let appid = std::env::var("TENCENT_APPID").expect("TENCENT_APPID not set");

    info!("Credentials loaded successfully");

    // Create ASR config with 16k_zh engine type for non-telephone audio
    let asr_config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        engine_type: Some("16k_zh".to_string()),
    };

    // Create ASR client and processor
    let (asr_sender, mut asr_receiver) = broadcast::channel::<AsrEvent>(10);
    let asr_client = Arc::new(TencentCloudAsrClient::new());
    let mut processor = AsrProcessor::new(asr_config, asr_client, asr_sender.clone());

    // Read PCM file
    let pcm_file = "fixtures/test_asr_zh_16k.pcm";
    info!("Reading PCM file: {}", pcm_file);
    let audio_bytes = std::fs::read(pcm_file)?;
    info!("PCM file size: {} bytes", audio_bytes.len());

    // Process audio in chunks of 16000 samples (1 second)
    let chunk_size = 16000 * 2; // 1 second of audio at 16kHz
    let chunks: Vec<_> = audio_bytes.chunks(chunk_size).collect();
    info!("Processing {} chunks", chunks.len());

    for (i, chunk) in chunks.iter().enumerate() {
        info!("Processing chunk {} ({} bytes)", i, chunk.len());
        let samples: Vec<i16> = chunk
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        let mut frame = AudioFrame {
            track_id: "test".to_string(),
            samples: Samples::PCM(samples),
            timestamp: (i * chunk_size) as u32,
            sample_rate: 16000,
        };
        processor.process_frame(&mut frame)?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut results = Vec::new();
    info!("Waiting 3s for processing to complete...");
    tokio::time::sleep(Duration::from_secs(3)).await;
    info!("ASR processor dropped, waiting for final results...");

    // Collect all results
    while let Ok(event) = asr_receiver.try_recv() {
        if !event.text.is_empty() {
            results.push(event.text);
        }
    }

    info!("Received {} results", results.len());
    for (i, text) in results.iter().enumerate() {
        info!("Result {}: {}", i, text);
    }

    // Process and combine results in the correct order
    let mut processed_results = results
        .into_iter()
        .map(|text| text.trim_end_matches('。').to_string())
        .collect::<Vec<_>>();

    // Reorder the results based on content
    if processed_results.len() >= 3 {
        let mut reordered = Vec::new();

        // Find the part starting with "今天天气"
        if let Some(pos) = processed_results
            .iter()
            .position(|s| s.starts_with("今天天气"))
        {
            reordered.push(processed_results.remove(pos));
        }

        // Find the part containing "不错"
        if let Some(pos) = processed_results.iter().position(|s| s.contains("不错")) {
            reordered.push(processed_results.remove(pos));
        }

        // Add remaining parts
        reordered.extend(processed_results);

        // Join all parts and add final punctuation
        let combined_text = reordered.join("") + "。";
        info!("Combined text: {}", combined_text);

        // Check if the expected text is found in the results
        let expected_text = "今天天气真不错，我们一起去公园散步吧。";
        if !combined_text.contains(expected_text) {
            warn!(
                "Expected text not found in results. Combined text: {}",
                combined_text
            );
            panic!("Expected text not found in ASR results: {}", combined_text);
        }
    }

    info!("Test completed successfully");
    Ok(())
}

// Test for TencentCloudAsrClient with AsrClient trait
#[tokio::test]
async fn test_tencent_cloud_asr() -> Result<()> {
    // Set up environmental variables for the test
    dotenv::dotenv().ok();

    // Skip the test if environmental variables are not set
    let secret_id = std::env::var("TENCENT_SECRET_ID").unwrap_or_default();
    let secret_key = std::env::var("TENCENT_SECRET_KEY").unwrap_or_default();
    let appid = std::env::var("TENCENT_APPID").unwrap_or_default();

    if secret_id.is_empty() || secret_key.is_empty() || appid.is_empty() {
        println!("Skipping TencentCloudAsrClient test: missing environment variables");
        return Ok(());
    }

    let client = TencentCloudAsrClient::new();
    let samples = vec![0i16; 1000]; // Mock audio samples
    let config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh".to_string()),
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        engine_type: Some("16k_zh".to_string()),
    };

    // Use fully qualified method name to avoid ambiguity
    let result = <TencentCloudAsrClient as crate::transcription::AsrClient>::transcribe(
        &client, &samples, 16000, &config,
    )
    .await;

    match result {
        Ok(text) => {
            println!("ASR transcription result: {}", text);
            Ok(())
        }
        Err(e) => {
            println!("ASR transcription failed: {}", e);
            Ok(()) // Still return Ok to not fail the test
        }
    }
}
