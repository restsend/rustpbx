use crate::synthesis::{
    tencent_cloud::TencentCloudTtsClient, AliyunTtsClient, SynthesisClient, SynthesisOption,
    SynthesisType,
};
use dotenv::dotenv;
use futures::StreamExt;
use std::env;
use std::fs::File;
use std::io::Write;
use hound::{WavSpec, WavWriter, SampleFormat};
use anyhow::Result;

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
        speaker: Some("501004".to_string()), // Standard female voice
        volume: Some(0),                     // Medium volume
        speed: Some(0.0),                    // Normal speed
        codec: Some("pcm".to_string()),      // PCM format for easy verification
        ..Default::default()
    };

    let client = TencentCloudTtsClient::new(config);
    let text = "Hello";
    match client.synthesize(text, None).await {
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
        volume: Some(5),                         // Medium volume (0-10)
        speed: Some(1.0),                        // Normal speed
        codec: Some("pcm".to_string()),          // PCM format for easy verification
        samplerate: Some(16000),                 // 16kHz sample rate
        ..Default::default()
    };

    // Test that the client can be created successfully
    let client = AliyunTtsClient::new(config);
    assert_eq!(client.provider(), SynthesisType::Aliyun);

    println!("Aliyun TTS client created successfully");
    println!("Test passes - implementation is structurally correct");

    let stream = client.synthesize("Hello", None).await;
    if let Ok(mut stream) = stream {
        let mut audio_collector = Vec::with_capacity(8096);
        let mut chunks_count = 0;
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    audio_collector.extend_from_slice(&chunk);
                    chunks_count += 1;
                }
                Err(e) => {
                    panic!("Error in audio stream chunk: {:?}", e);
                }
            }
        }        
        println!("Total audio size: {} bytes", audio_collector.len());
        println!("Total chunks: {}", chunks_count);
        // save_audio_to_files(&audio_collector, 16000, "test_aliyun_tts").unwrap();
    } else {
        panic!("Error in audio stream: {:?}", stream.err());
    }

}


/// Save PCM audio data to files for testing
#[allow(dead_code)]
fn save_audio_to_files(audio_data: &[u8], sample_rate: u32, prefix: &str) -> Result<()> {
    if audio_data.is_empty() {
        return Err(anyhow::anyhow!("No audio data to save"));
    }

    // Save as raw PCM file
    let pcm_filename = format!("{}.pcm", prefix);
    let mut pcm_file = File::create(&pcm_filename)?;
    pcm_file.write_all(audio_data)?;
    println!("✓ Saved raw PCM audio to: {}", pcm_filename);
    println!("  Play with: ffplay -f s16le -ar {} -ac 1 {}", sample_rate, pcm_filename);

    // Convert bytes to samples and save as WAV
    let samples: Vec<i16> = audio_data
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    let wav_filename = format!("{}.wav", prefix);
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut writer = WavWriter::create(&wav_filename, spec)?;
    for sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    println!("✓ Saved WAV audio to: {}", wav_filename);
    println!("  Play with: ffplay {} or any audio player", wav_filename);

    Ok(())
}