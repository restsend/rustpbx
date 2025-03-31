use crate::{
    media::track::file::read_wav_file,
    transcription::{
        tencent_cloud::TencentCloudAsrClientBuilder, TranscriptionClient, TranscriptionConfig,
    },
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
    let (samples, sample_rate) = read_wav_file(audio_path).expect("Failed to read WAV file");
    println!(
        "Read {} samples {}HZ from WAV file ({} seconds of audio)",
        samples.len(),
        sample_rate,
        samples.len() as f32 / sample_rate as f32
    );
    // Send audio data in chunks
    let chunk_size = 3200; // 100ms of audio at 16kHz
    let chunks: Vec<_> = samples.chunks(chunk_size).collect();
    println!("Starting to send {} chunks of audio data", chunks.len());

    for chunk in chunks.iter() {
        client
            .send_audio(chunk)
            .expect("Failed to send audio chunk");
    }
    // Wait for transcription result with timeout
    let timeout_duration = Duration::from_secs(5);
    let result_fut = async {
        let mut frames = Vec::new();
        while let Some(frame) = client.next().await {
            let text = frame.text.clone();
            if frame.is_final {
                frames.push(frame);
                if text.contains("你好") {
                    break;
                }
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
        .map(|f| f.text.clone())
        .collect::<Vec<_>>()
        .join("");

    println!("Final transcription result: {}", final_text);
    assert!(
        final_text.contains("你好"),
        "Expected transcription to contain booking or course"
    );
}
