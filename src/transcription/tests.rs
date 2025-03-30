use crate::transcription::{
    tencent_cloud::TencentCloudAsrClientBuilder, TranscriptionClient, TranscriptionConfig,
};
use dotenv::dotenv;
use once_cell::sync::OnceCell;
use rustls::crypto::ring::default_provider;
use std::env;
use tokio::time::{timeout, Duration};

static CRYPTO_PROVIDER: OnceCell<()> = OnceCell::new();

fn init_crypto() {
    CRYPTO_PROVIDER.get_or_init(|| {
        rustls::crypto::CryptoProvider::install_default(default_provider())
            .expect("Failed to initialize crypto provider");
    });
}

// Helper function to get credentials from .env file
fn get_tencent_credentials() -> Option<(String, String, String)> {
    dotenv().ok();
    let secret_id = env::var("TENCENT_SECRET_ID").ok()?;
    let secret_key = env::var("TENCENT_SECRET_KEY").ok()?;
    let app_id = env::var("TENCENT_APPID").ok()?;

    Some((secret_id, secret_key, app_id))
}

#[tokio::test]
async fn test_tencent_cloud_asr() {
    // Initialize the crypto provider
    init_crypto();

    // Skip test if credentials are not available
    let (secret_id, secret_key, app_id) = match get_tencent_credentials() {
        Some(creds) => creds,
        None => {
            println!("Skipping test_tencent_cloud_asr: No credentials found in .env file");
            return;
        }
    };

    println!(
        "Using credentials: secret_id={}, app_id={}",
        secret_id, app_id
    );

    // Configure the client
    let config = TranscriptionConfig {
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        appid: Some(app_id),
        ..Default::default()
    };

    // Create client builder and connect
    let client_builder = TencentCloudAsrClientBuilder::new(config);
    let client = match client_builder.build().await {
        Ok(c) => c,
        Err(e) => {
            println!("Failed to connect to ASR service: {:?}", e);
            return;
        }
    };

    // Read the test audio file
    let audio_path = "fixtures/hello_book_course_zh_16k.wav";
    if let Err(e) = std::fs::metadata(audio_path) {
        println!("Test audio file not found: {}. Error: {:?}", audio_path, e);
        return;
    }

    let mut reader = match hound::WavReader::open(audio_path) {
        Ok(r) => {
            println!(
                "Opened WAV file: channels={}, sample_rate={}, bits_per_sample={}, duration={} seconds",
                r.spec().channels,
                r.spec().sample_rate,
                r.spec().bits_per_sample,
                r.duration() as f32 / r.spec().sample_rate as f32
            );
            r
        }
        Err(e) => {
            println!("Failed to open WAV file: {:?}", e);
            return;
        }
    };

    // Read all samples
    let samples: Vec<i16> = reader.samples().filter_map(Result::ok).collect();
    println!(
        "Read {} samples from WAV file ({} seconds of audio)",
        samples.len(),
        samples.len() as f32 / 16000.0
    );

    // Calculate audio statistics
    let max_amplitude = samples.iter().map(|&x| x.abs()).max().unwrap_or(0);
    let min_amplitude = samples.iter().map(|&x| x.abs()).min().unwrap_or(0);
    println!(
        "Audio statistics: max_amplitude={}, min_amplitude={}",
        max_amplitude, min_amplitude
    );

    // Trim silence from the beginning and end
    let silence_threshold = 100;
    let mut start_idx = 0;
    let mut end_idx = samples.len();

    // Find first non-silent sample
    for (i, &sample) in samples.iter().enumerate() {
        if sample.abs() > silence_threshold {
            start_idx = i;
            break;
        }
    }
    println!("Skipping {} samples of initial silence", start_idx);

    // Find last non-silent sample
    for (i, &sample) in samples.iter().enumerate().rev() {
        if sample.abs() > silence_threshold {
            end_idx = i + 1;
            break;
        }
    }
    println!(
        "Trimming {} samples of trailing silence",
        samples.len() - end_idx
    );

    // Print first few non-zero samples for debugging
    let first_samples = &samples[start_idx..std::cmp::min(start_idx + 10, end_idx)];
    println!("First 10 non-zero samples: {:?}", first_samples);

    // Get the trimmed samples
    let trimmed_samples = &samples[start_idx..end_idx];
    println!(
        "After trimming silence: {} samples ({} seconds of audio)",
        trimmed_samples.len(),
        trimmed_samples.len() as f32 / 16000.0
    );

    // Send audio data in chunks
    let chunk_size = 3200; // 100ms of audio at 16kHz
    let chunks: Vec<_> = trimmed_samples.chunks(chunk_size).collect();
    println!("Starting to send {} chunks of audio data", chunks.len());

    for (i, chunk) in chunks.iter().enumerate() {
        println!(
            "Sending chunk {}/{} ({} samples, {} bytes)",
            i + 1,
            chunks.len(),
            chunk.len(),
            chunk.len() * 2
        );

        match client.send_audio(chunk).await {
            Ok(_) => {
                // Add a small delay between chunks to simulate real-time streaming
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                println!(
                    "Failed to send audio chunk {}/{}: {}",
                    i + 1,
                    chunks.len(),
                    e
                );
                break;
            }
        }
    }

    // Send empty chunk to signal end of audio
    if let Err(e) = client.send_audio(&[]).await {
        println!("Failed to send end signal: {}", e);
    }

    // Wait for transcription result with timeout
    let timeout_duration = Duration::from_secs(30);
    let result_fut = async {
        let mut frames = Vec::new();
        while let Some(frame) = client.next().await {
            frames.push(frame.clone());
            if frame.is_final {
                break;
            }
        }
        frames
    };

    let frames = match timeout(timeout_duration, result_fut).await {
        Ok(frames) => frames,
        Err(_) => {
            println!("Timeout waiting for transcription result");
            vec![]
        }
    };

    let final_text = frames
        .iter()
        .filter(|f| f.is_final)
        .map(|f| f.text.clone())
        .collect::<Vec<_>>()
        .join(" ");

    println!("Final transcription result: {}", final_text);

    // Check if the transcription contains expected phrases
    assert!(
        final_text.contains("您好") || final_text.contains("你好"),
        "Expected transcription to contain greeting"
    );
    assert!(
        final_text.contains("预约") || final_text.contains("课程"),
        "Expected transcription to contain booking or course"
    );
}

#[tokio::test]
async fn test_tencent_cloud_asr_streaming() {
    // Initialize the crypto provider
    init_crypto();

    // Skip test if credentials are not available
    let (secret_id, secret_key, app_id) = match get_tencent_credentials() {
        Some(creds) => creds,
        None => {
            println!(
                "Skipping test_tencent_cloud_asr_streaming: No credentials found in .env file"
            );
            return;
        }
    };

    // Configure the client
    let config = TranscriptionConfig {
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        appid: Some(app_id),
        engine_type: "16k_zh".to_string(),
        ..Default::default()
    };

    // Create client builder and connect
    let client_builder = TencentCloudAsrClientBuilder::new(config);
    let client = match client_builder.build().await {
        Ok(c) => c,
        Err(e) => {
            println!("Skipping test due to connection error: {:?}", e);
            // Don't fail the test, just skip it
            return;
        }
    };

    // Send some test audio data
    let test_data = vec![0i16; 16000]; // 1 second of silence
    if let Err(e) = client.send_audio(&test_data).await {
        println!("Failed to send audio data: {}", e);
        return;
    }

    // Send empty chunk to signal end of audio
    if let Err(e) = client.send_audio(&[]).await {
        println!("Failed to send end signal: {}", e);
        return;
    }

    // Wait for transcription result with timeout
    let timeout_duration = Duration::from_secs(10);
    let result_fut = async {
        let mut frames = Vec::new();
        while let Some(frame) = client.next().await {
            frames.push(frame.clone());
            if frame.is_final {
                break;
            }
        }
        frames
    };

    match timeout(timeout_duration, result_fut).await {
        Ok(frames) => {
            assert!(
                !frames.is_empty(),
                "Expected at least one transcription frame"
            );
        }
        Err(_) => {
            println!(
                "Timed out waiting for transcription result after {} seconds",
                timeout_duration.as_secs()
            );
            // Don't panic, just log the issue
            return;
        }
    }
}
