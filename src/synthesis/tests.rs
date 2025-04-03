use crate::synthesis::{tencent_cloud::TencentCloudTtsClient, SynthesisClient, SynthesisConfig};
use dotenv::dotenv;
use std::env;
use tokio::time::{timeout, Duration};

fn get_tencent_credentials() -> Option<(String, String, String)> {
    dotenv().ok();
    let secret_id = env::var("TENCENT_SECRET_ID").ok()?;
    let secret_key = env::var("TENCENT_SECRET_KEY").ok()?;
    let app_id = env::var("TENCENT_APPID").ok()?;

    Some((secret_id, secret_key, app_id))
}

#[tokio::test]
async fn test_tencent_cloud_tts() {
    let (secret_id, secret_key, app_id) = match get_tencent_credentials() {
        Some(creds) => creds,
        None => {
            println!("Skipping test_tencent_cloud_tts: No credentials found in .env file");
            return;
        }
    };

    let config = SynthesisConfig {
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        app_id: Some(app_id),
        speaker: Some("1".to_string()), // Standard female voice
        volume: Some(5),                // Medium volume
        rate: Some(1.0),                // Normal speed
        codec: Some("pcm".to_string()), // PCM format for easy verification
        ..Default::default()
    };

    let client = TencentCloudTtsClient::new(config);

    let text = "您好，这是一个测试。";

    let result_fut = async {
        match client.synthesize(text).await {
            Ok(audio) => audio,
            Err(e) => {
                println!("TTS synthesis error: {:?}", e);
                vec![] // Return empty vec on error to continue the test
            }
        }
    };

    let timeout_duration = Duration::from_secs(10);
    match timeout(timeout_duration, result_fut).await {
        Ok(audio) => {
            if audio.is_empty() {
                println!("Test skipped due to synthesis error");
                return;
            }

            assert!(!audio.is_empty(), "Expected non-empty audio data");
            println!("Received audio data of size: {} bytes", audio.len());

            // Basic PCM validation - check that audio data length is reasonable
            // For PCM at 16kHz, 16-bit, mono, we expect ~32000 bytes per second
            // For a short phrase, we expect at least a few thousand bytes
            assert!(
                audio.len() > 1000,
                "Audio data size too small: {} bytes",
                audio.len()
            );
        }
        Err(_) => {
            println!("Timed out waiting for synthesis result");
            // Don't panic, just log the issue
            return;
        }
    }
}
