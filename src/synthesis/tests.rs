use crate::synthesis::deepgram::DeepegramTtsClient;
use crate::synthesis::{
    AliyunTtsClient, SynthesisOption, SynthesisType, tencent_cloud::TencentCloudTtsClient,
};
use crate::synthesis::{
    SynthesisClient, SynthesisEvent, TencentCloudTtsBasicClient, strip_emoji_chars,
};
use dotenv::dotenv;
use futures::StreamExt;
use std::env;
use std::time::Duration;

async fn test_tts_basic(client: &mut dyn SynthesisClient) {
    let timeout = tokio::time::sleep(Duration::from_secs(5));
    let stream = client
        .start()
        .await
        .expect("Failed to start TTS stream")
        .take_until(timeout);
    tokio::pin!(stream);
    client
        .synthesize("Hello, this is a test.", None, None)
        .await
        .expect("Failed to synthesize text");
    client.stop().await.expect("Failed to stop TTS stream");
    let mut total_size = 0;
    let mut finished = false;
    let mut error_occurred = false;
    while let Some((_, event)) = stream.next().await {
        match event {
            Ok(SynthesisEvent::AudioChunk(audio)) => {
                total_size += audio.len();
            }
            Ok(SynthesisEvent::Subtitles(_)) => {}
            Ok(SynthesisEvent::Finished) => {
                finished = true;
            }
            Err(e) => {
                tracing::error!("error during tts: {}", e);
                error_occurred = true;
            }
        }
    }
    assert!(total_size > 16000, "no audio chunk received");
    assert!(finished, "tts not finished");
    assert!(!error_occurred, "error during tts");
}

async fn test_multiple_tts_commands_streaming(client: &mut dyn SynthesisClient) {
    let mut stream = client.start().await.expect("Failed to start TTS stream");
    client
        .synthesize("Hello, this is no.0 test", None, None)
        .await
        .expect("Failed to synthesize text");
    client
        .synthesize("Hello, this is no.1 test", None, None)
        .await
        .expect("Failed to synthesize text");
    client
        .synthesize("Hello, this is no.2 test", None, None)
        .await
        .expect("Failed to synthesize text");
    client.stop().await.expect("Failed to stop TTS stream");
    let mut total_size = 0;
    let mut finished = false;
    let mut error_occurred = false;
    let mut any_cmd_seq = None;
    while let Some((cmd_seq, chunk_result)) = stream.next().await {
        any_cmd_seq = cmd_seq.or(any_cmd_seq);
        match chunk_result {
            Ok(SynthesisEvent::AudioChunk(audio)) => {
                total_size += audio.len();
            }
            Ok(SynthesisEvent::Finished) => {
                finished = true;
            }
            Ok(SynthesisEvent::Subtitles(_)) => {}
            Err(e) => {
                tracing::error!("error during tts: {}", e);
                error_occurred = true;
            }
        }
    }
    assert!(total_size > 16000, "no audio chunk received");
    assert!(finished, "tts not finished");
    assert!(!error_occurred, "error occurred during tts");
    assert_eq!(
        any_cmd_seq, None,
        "cmd_seq should be None in streaming mode"
    );
}

async fn test_multiple_tts_commands_non_streaming(client: &mut dyn SynthesisClient) {
    let mut stream = client.start().await.expect("Failed to start TTS stream");
    client
        .synthesize("Hello, this is no.0 test", Some(0), None)
        .await
        .expect("Failed to synthesize text");
    client
        .synthesize("Hello, this is no.1 test", Some(1), None)
        .await
        .expect("Failed to synthesize text");
    client
        .synthesize("Hello, this is no.2 test", Some(2), None)
        .await
        .expect("Failed to synthesize text");
    client.stop().await.expect("Failed to stop TTS stream");
    let mut total_size = 0;
    let mut finished_count = 0;
    let mut error_occurred = false;
    let mut any_cmd_seq = None;
    while let Some((cmd_seq, chunk_result)) = stream.next().await {
        any_cmd_seq = cmd_seq.or(any_cmd_seq);
        match chunk_result {
            Ok(SynthesisEvent::AudioChunk(audio)) => {
                total_size += audio.len();
            }
            Ok(SynthesisEvent::Finished) => {
                finished_count += 1;
            }
            Ok(SynthesisEvent::Subtitles(_)) => {}
            Err(_) => {
                error_occurred = true;
            }
        }
    }
    assert!(total_size > 16000, "no audio chunk received");
    assert_eq!(finished_count, 3, "some tts commands not finished");
    assert!(!error_occurred, "error occurred during tts");
    assert!(
        any_cmd_seq.is_some(),
        "cmd_seq should be Some in non-streaming mode"
    );
}

fn get_tencent_credentials() -> Option<(String, String, String)> {
    dotenv().ok();
    let secret_id = env::var("TENCENT_SECRET_ID").ok()?;
    let secret_key = env::var("TENCENT_SECRET_KEY").ok()?;
    let app_id = env::var("TENCENT_APPID").ok()?;

    Some((secret_id, secret_key, app_id))
}

fn get_aliyun_credentials() -> Option<String> {
    dotenv().ok();
    env::var("DASHSCOPE_API_KEY").ok()
}

fn get_deepgram_credentials() -> Option<String> {
    dotenv().ok();
    env::var("DEEPGRAM_API_KEY").ok()
}

#[tokio::test]
async fn test_tencent_cloud_tts() {
    // Initialize crypto provider
    rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider()).ok();

    let (secret_id, secret_key, app_id) = match get_tencent_credentials() {
        Some(creds) => creds,
        None => {
            tracing::warn!("Skipping test_tencent_cloud_tts: No credentials found in .env file");
            return;
        }
    };

    let config = SynthesisOption {
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        app_id: Some(app_id),
        speaker: Some("501001".to_string()), // Standard female voice
        volume: Some(0),                     // Medium volume
        speed: Some(0.0),                    // Normal speed
        codec: Some("pcm".to_string()),      // PCM format for easy verification
        max_concurrent_tasks: Some(3),       // 3 concurrent tasks
        ..Default::default()
    };

    // test real time client
    let mut realtime_client = TencentCloudTtsClient::create(false, &config).unwrap();
    test_tts_basic(realtime_client.as_mut()).await;
    test_multiple_tts_commands_non_streaming(realtime_client.as_mut()).await;

    // test stereaming client
    let mut streaming_client = TencentCloudTtsClient::create(true, &config).unwrap();
    test_tts_basic(streaming_client.as_mut()).await;
    test_multiple_tts_commands_streaming(streaming_client.as_mut()).await;

    let mut basic_client = TencentCloudTtsBasicClient::create(false, &config).unwrap();
    test_tts_basic(basic_client.as_mut()).await;
    test_multiple_tts_commands_streaming(basic_client.as_mut()).await;
    test_multiple_tts_commands_non_streaming(basic_client.as_mut()).await;
}

#[tokio::test]
async fn test_aliyun_tts() {
    // Initialize crypto provider
    rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider()).ok();

    let api_key = match get_aliyun_credentials() {
        Some(key) => key,
        None => {
            println!("Skipping test_aliyun_tts: No DASHSCOPE_API_KEY found in .env file");
            return;
        }
    };

    let config = SynthesisOption {
        provider: Some(SynthesisType::Aliyun),
        secret_key: Some(api_key),
        speaker: Some("longyumi_v2".to_string()), // Default voice
        volume: Some(5),                          // Medium volume (0-10)
        speed: Some(1.0),                         // Normal speed
        codec: Some("pcm".to_string()),           // PCM format for easy verification
        samplerate: Some(16000),                  // 16kHz sample rate
        max_concurrent_tasks: Some(3),            // 3 concurrent tasks
        ..Default::default()
    };

    let mut non_streaming_client = AliyunTtsClient::create(false, &config).unwrap();
    test_tts_basic(non_streaming_client.as_mut()).await;
    test_multiple_tts_commands_non_streaming(non_streaming_client.as_mut()).await;

    let mut streaming_client = AliyunTtsClient::create(true, &config).unwrap();
    test_tts_basic(streaming_client.as_mut()).await;
    test_multiple_tts_commands_streaming(streaming_client.as_mut()).await;
}

#[tokio::test]
async fn test_deepgram_tts() {
    rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider()).ok();

    let api_key = match get_deepgram_credentials() {
        Some(key) => key,
        None => {
            return;
        }
    };

    let config = SynthesisOption {
        provider: Some(SynthesisType::Deepgram),
        secret_key: Some(api_key),
        max_concurrent_tasks: Some(3),
        ..Default::default()
    };

    let mut non_streaming_client = DeepegramTtsClient::create(false, &config).unwrap();
    test_tts_basic(non_streaming_client.as_mut()).await;
    test_multiple_tts_commands_non_streaming(non_streaming_client.as_mut()).await;

    let mut streaming_client = DeepegramTtsClient::create(true, &config).unwrap();
    test_tts_basic(streaming_client.as_mut()).await;
    test_multiple_tts_commands_streaming(streaming_client.as_mut()).await;
}

#[tokio::test]
async fn test_emoji_strip() {
    let text = "Hello, world! 😊 This is a test with emojis( 🚀🔥.)";
    let stripped = strip_emoji_chars(text);
    assert_eq!(stripped, "Hello, world!  This is a test with emojis( .)");

    let text = "2025-09-25， 18点02分41秒";
    let stripped_no_emoji = strip_emoji_chars(text);
    assert_eq!(stripped_no_emoji, "2025-09-25， 18点02分41秒");
}
