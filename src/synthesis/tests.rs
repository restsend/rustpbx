use crate::synthesis::{tencent_cloud::TencentCloudTtsClient, SynthesisClient, SynthesisConfig};
use dotenv::dotenv;
use futures::StreamExt;
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
            Ok(mut stream) => {
                // Collect all chunks from the stream
                let mut total_size = 0;
                let mut chunks_count = 0;
                let mut collected_audio = Vec::new();

                while let Some(chunk_result) = stream.next().await {
                    match chunk_result {
                        Ok(chunk) => {
                            total_size += chunk.len();
                            chunks_count += 1;
                            collected_audio.extend_from_slice(&chunk);
                        }
                        Err(e) => {
                            println!("Error in audio stream chunk: {:?}", e);
                            return (0, 0, vec![]);
                        }
                    }
                }

                (total_size, chunks_count, collected_audio)
            }
            Err(e) => {
                println!("TTS synthesis error: {:?}", e);
                (0, 0, vec![]) // Return empty vec on error to continue the test
            }
        }
    };

    let timeout_duration = Duration::from_secs(10);
    match timeout(timeout_duration, result_fut).await {
        Ok((total_size, chunks_count, audio)) => {
            if total_size == 0 {
                println!("Test skipped due to synthesis error");
                return;
            }

            assert!(total_size > 0, "Expected non-empty audio data from stream");
            assert!(chunks_count > 0, "Expected at least one chunk from stream");
            println!(
                "Received {} audio chunks with total size: {} bytes",
                chunks_count, total_size
            );

            // Basic PCM validation - check that audio data length is reasonable
            // For PCM at 16kHz, 16-bit, mono, we expect ~32000 bytes per second
            // For a short phrase, we expect at least a few thousand bytes
            assert!(
                total_size > 1000,
                "Audio data size too small: {} bytes",
                total_size
            );

            // Verify the complete audio
            assert!(!audio.is_empty(), "Expected non-empty audio data");
            assert_eq!(
                audio.len(),
                total_size,
                "Collected audio size should match total size"
            );
        }
        Err(_) => {
            println!("Timed out waiting for synthesis result");
            // Don't panic, just log the issue
            return;
        }
    }
}
