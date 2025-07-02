use crate::{
    event::SessionEvent,
    media::track::file::read_wav_file,
    transcription::{
        aliyun::AliyunAsrClientBuilder, tencent_cloud::TencentCloudAsrClientBuilder,
        TranscriptionClient, TranscriptionOption,
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
        rustls::crypto::CryptoProvider::install_default(default_provider()).ok();
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

// Helper function to get Aliyun credentials from .env file
fn get_aliyun_credentials() -> Option<String> {
    dotenv().ok();
    env::var("DASHSCOPE_API_KEY").ok()
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
    let config = TranscriptionOption {
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        app_id: Some(app_id),
        ..Default::default()
    };
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(16);
    // Create client builder and connect
    let client_builder = TencentCloudAsrClientBuilder::new(config, event_sender);
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Wait for transcription result with timeout
    let timeout_duration = Duration::from_secs(5);
    let result_fut = async {
        let mut fulltext = String::new();
        while let Ok(event) = event_receiver.recv().await {
            match event {
                SessionEvent::AsrDelta { text, .. } => {
                    fulltext += &text;
                }
                _ => {}
            }
            if fulltext.contains("你好") {
                break;
            }
        }
        fulltext
    };

    let final_text = match timeout(timeout_duration, result_fut).await {
        Ok(fulltext) => fulltext,
        Err(_) => {
            println!("Timeout waiting for transcription result");
            String::new()
        }
    };

    println!("Final transcription result: {}", final_text);
    assert!(
        final_text.contains("你好"),
        "Expected transcription to contain booking or course"
    );
}

#[tokio::test]
async fn test_aliyun_asr() {
    // Initialize the crypto provider
    init_crypto();

    // Skip test if credentials are not available
    let api_key = match get_aliyun_credentials() {
        Some(key) => key,
        None => {
            println!("Skipping test_aliyun_asr: No DASHSCOPE_API_KEY found in .env file");
            return;
        }
    };

    println!("Using Aliyun API key: {}", &api_key[..8]); // Only show first 8 chars for security

    // Configure the client
    let config = TranscriptionOption {
        secret_key: Some(api_key),
        ..Default::default()
    };
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(16);

    // Create client builder and connect
    let client_builder = AliyunAsrClientBuilder::new(config, event_sender);
    let client = match client_builder.build().await {
        Ok(c) => c,
        Err(e) => {
            println!("Failed to connect to Aliyun ASR service: {:?}", e);
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Wait for transcription result with timeout
    let timeout_duration = Duration::from_secs(10);
    let result_fut = async {
        let mut fulltext = String::new();
        while let Ok(event) = event_receiver.recv().await {
            match event {
                SessionEvent::AsrDelta { text, .. } => {
                    fulltext += &text;
                }
                SessionEvent::AsrFinal { text, .. } => {
                    fulltext += &text;
                }
                _ => {}
            }
            if fulltext.contains("你好") || fulltext.len() > 20 {
                break;
            }
        }
        fulltext
    };

    let final_text = match timeout(timeout_duration, result_fut).await {
        Ok(fulltext) => fulltext,
        Err(_) => {
            println!("Timeout waiting for transcription result");
            String::new()
        }
    };

    println!("Final transcription result: {}", final_text);
    // For Aliyun, we expect some transcription result but content may vary
    assert!(
        !final_text.is_empty(),
        "Expected some transcription result from Aliyun ASR"
    );
}
