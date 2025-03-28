use std::sync::Arc;

use rustpbx::synthesis::{TencentCloudTtsClient, TtsClient, TtsConfig, TtsEvent, TtsProcessor};
use rustpbx::transcription::{AsrConfig, AsrEvent, AsrProcessor, TencentCloudAsrClient};

use anyhow::Result;
use dotenv::dotenv;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> Result<()> {
    // Set up logging
    tracing_subscriber::fmt::init();

    // Load .env file if it exists
    dotenv().ok();

    // Get credentials from environment variables
    let secret_id = match std::env::var("TENCENT_SECRET_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            println!("TENCENT_SECRET_ID env var not set or empty. Please set it in .env file");
            return Ok(());
        }
    };

    let secret_key = match std::env::var("TENCENT_SECRET_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            println!("TENCENT_SECRET_KEY env var not set or empty. Please set it in .env file");
            return Ok(());
        }
    };

    let appid = match std::env::var("TENCENT_APPID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            println!("TENCENT_APPID env var not set or empty. Please set it in .env file");
            return Ok(());
        }
    };

    // Create TTS client and processor
    println!("Initializing TTS client...");
    let tts_config = TtsConfig {
        url: "".to_string(),          // Not used with TencentCloud client
        voice: Some("1".to_string()), // Voice type - 1 is typically the first voice option
        rate: Some(1.0),              // Normal speed
        appid: Some(appid.clone()),
        secret_id: Some(secret_id.clone()),
        secret_key: Some(secret_key.clone()),
        volume: Some(5),                // Volume level (0-10)
        speaker: Some(1),               // Speaker type
        codec: Some("pcm".to_string()), // PCM format
    };

    let (tts_sender, _) = broadcast::channel::<TtsEvent>(10);
    let tts_client = Arc::new(TencentCloudTtsClient::new());
    let tts_processor = TtsProcessor::new(tts_config, tts_client, tts_sender);

    // Create ASR client and processor
    println!("Initializing ASR client...");
    let asr_config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        engine_type: Some("16k_zh".to_string()), // Chinese 16kHz model
    };

    let (asr_sender, mut asr_receiver) = broadcast::channel::<AsrEvent>(10);
    let asr_client = Arc::new(TencentCloudAsrClient::new());
    let _asr_processor = AsrProcessor::new(asr_config, asr_client, asr_sender);

    // Spawn a task to listen for ASR events
    tokio::spawn(async move {
        while let Ok(event) = asr_receiver.recv().await {
            println!("ASR Event: {} (final: {})", event.text, event.is_final);
        }
    });

    // Synthesize text to speech
    println!("Synthesizing text to speech...");
    let text = "欢迎使用腾讯云语音服务";
    let result = tts_processor.synthesize(text).await?;

    // Save the audio to a file
    std::fs::write("tts_output.pcm", &result)?;
    println!(
        "TTS output saved to tts_output.pcm ({} bytes)",
        result.len()
    );

    // In a real application, you would connect the ASR processor to an audio source
    println!("Example completed successfully.");

    Ok(())
}
