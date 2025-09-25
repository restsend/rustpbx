use crate::synthesis::SynthesisEvent;
use crate::synthesis::{
    AliyunTtsClient, SynthesisOption, SynthesisType, tencent_cloud::TencentCloudTtsClient,
};
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

fn get_aliyun_credentials() -> Option<String> {
    dotenv().ok();
    env::var("DASHSCOPE_API_KEY").ok()
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

    let config = SynthesisOption {
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        app_id: Some(app_id),
        speaker: Some("501001".to_string()), // Standard female voice
        volume: Some(0),                     // Medium volume
        speed: Some(0.0),                    // Normal speed
        codec: Some("pcm".to_string()),      // PCM format for easy verification
        ..Default::default()
    };

    // test real time client
    let mut realtime_client = TencentCloudTtsClient::create(false, &config).unwrap();
    let text = "Hello, this is a test of Tencent Cloud TTS.";
    let mut stream = realtime_client
        .start()
        .await
        .expect("Failed to start TTS stream");
    realtime_client
        .synthesize(text, Some(0), None)
        .await
        .expect("Failed to synthesize text");

    let mut total_size = 0;
    let mut subtitles_count = 0;
    let mut finished = false;
    let mut last_cmd_seq = None;
    while let Some((cmd_seq, chunk_result)) = stream.next().await {
        last_cmd_seq = cmd_seq;
        match chunk_result {
            Ok(SynthesisEvent::AudioChunk(audio)) => {
                total_size += audio.len();
            }
            Ok(SynthesisEvent::Finished { .. }) => {
                finished = true;
                break;
            }
            Ok(SynthesisEvent::Subtitles(subtitles)) => {
                subtitles_count += subtitles.len();
            }
            Err(_) => {
                break;
            }
        }
    }

    assert!(total_size > 0);
    assert!(subtitles_count > 0);
    assert!(finished);
    assert_eq!(last_cmd_seq, Some(0));

    // test stereaming client
    let mut streaming_client = TencentCloudTtsClient::create(true, &config).unwrap();
    let text = "Hello, this is a test of Tencent Cloud TTS.";
    let mut stream = streaming_client
        .start()
        .await
        .expect("Failed to start TTS stream");
    streaming_client
        .synthesize(text, None, None)
        .await
        .expect("Failed to synthesize text");
    streaming_client.stop().await.unwrap();

    // Collect all chunks from the stream
    let mut total_size = 0;
    let mut subtitles_count = 0;
    let mut finished = false;
    let mut last_cmd_seq = None;
    while let Some((cmd_seq, chunk_result)) = stream.next().await {
        last_cmd_seq = cmd_seq;
        match chunk_result {
            Ok(SynthesisEvent::AudioChunk(audio)) => {
                total_size += audio.len();
            }
            Ok(SynthesisEvent::Finished { .. }) => {
                finished = true;
                break;
            }
            Ok(SynthesisEvent::Subtitles(subtitles)) => {
                subtitles_count += subtitles.len();
            }
            Err(_) => {
                break;
            }
        }
    }

    assert!(total_size > 0);
    assert!(subtitles_count > 0);
    assert!(finished);
    assert!(last_cmd_seq.is_none());
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
        ..Default::default()
    };

    let mut non_streaming_client = AliyunTtsClient::create(false, &config).unwrap();
    let mut stream = non_streaming_client
        .start()
        .await
        .expect("Failed to start Aliyun TTS stream");

    non_streaming_client
        .synthesize("Hello, how are you?", Some(0), None)
        .await
        .expect("Failed to synthesize text");

    let mut total_size = 0;
    let mut finished = false;
    let mut last_cmd_seq = None;
    while let Some((cmd_seq, res)) = stream.next().await {
        last_cmd_seq = cmd_seq;
        match res {
            Ok(event) => {
                match event {
                    SynthesisEvent::AudioChunk(chunk) => {
                        total_size += chunk.len();
                    }
                    SynthesisEvent::Finished { .. } => {
                        finished = true;
                        break;
                    }
                    SynthesisEvent::Subtitles { .. } => {
                        // ignore progress
                    }
                }
            }
            Err(_) => {
                break;
            }
        }
    }

    assert!(total_size > 0);
    assert!(finished);
    assert_eq!(last_cmd_seq, Some(0));

    let mut streaming_client = AliyunTtsClient::create(true, &config).unwrap();
    let mut stream = streaming_client
        .start()
        .await
        .expect("Failed to start Aliyun TTS stream");
    streaming_client
        .synthesize("Hello, how are you?", None, None)
        .await
        .expect("Failed to synthesize text");
    streaming_client.stop().await.unwrap();

    let mut total_size = 0;
    let mut finished = false;
    let mut last_cmd_seq = None;
    while let Some((cmd_seq, chunk_result)) = stream.next().await {
        last_cmd_seq = cmd_seq;
        match chunk_result {
            Ok(SynthesisEvent::AudioChunk(audio)) => {
                total_size += audio.len();
            }
            Ok(SynthesisEvent::Finished { .. }) => {
                finished = true;
                break;
            }
            Ok(SynthesisEvent::Subtitles { .. }) => {}
            Err(_) => {
                break;
            }
        }
    }

    assert!(total_size > 0);
    assert!(finished);
    assert!(last_cmd_seq.is_none());
}
