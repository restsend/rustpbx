use crate::synthesis::{tencent_cloud::TencentCloudTtsClient, SynthesisClient, SynthesisConfig};
use dotenv::dotenv;
use futures::StreamExt;
use std::env;

fn get_tencent_credentials() -> Option<(String, String, String)> {
    dotenv().ok();
    let secret_id = env::var("TENCENT_SECRET_ID").ok()?;
    let secret_key = env::var("TENCENT_SECRET_KEY").ok()?;
    let app_id = env::var("TENCENT_APPID").ok()?;

    Some((secret_id, secret_key, app_id))
}

#[tokio::test]
async fn test_tencent_cloud_tts() {
    // Initialize crypto provider
    rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider()).ok();

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
        speaker: Some("101001".to_string()), // Standard female voice
        volume: Some(0),                     // Medium volume
        speed: Some(0.0),                    // Normal speed
        codec: Some("pcm".to_string()),      // PCM format for easy verification
        ..Default::default()
    };

    let client = TencentCloudTtsClient::new(config);
    let text = "Hello";
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
                        panic!("Error in audio stream chunk: {:?}", e);
                    }
                }
            }
            println!("Total audio size: {} bytes", total_size);
            println!("Total chunks: {}", chunks_count);
        }
        Err(e) => {
            panic!("TTS synthesis error: {:?}", e);
        }
    };
}
