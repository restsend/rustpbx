use std::sync::Arc;

use rustpbx::media::processor::{AudioFrame, Processor, Samples};
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
    let test_text = "今天天气真不错，我们一起去公园散步吧";
    println!("Original text: {}", test_text);
    let result = tts_processor.synthesize(test_text).await?;

    // Save the audio to a file
    let pcm_file = "test_tts_output.pcm";
    std::fs::write(pcm_file, &result)?;
    println!("TTS output saved to {} ({} bytes)", pcm_file, result.len());

    // Now test ASR with the generated audio file
    println!("\nTesting ASR with the generated audio file...");
    let audio_data = std::fs::read(pcm_file)?;

    // Create chunks to simulate streaming audio (16KB chunks)
    const CHUNK_SIZE: usize = 16 * 1024;
    let chunks: Vec<_> = audio_data.chunks(CHUNK_SIZE).collect();

    // Process each chunk through ASR
    for (i, chunk) in chunks.iter().enumerate() {
        let mut frame = AudioFrame {
            track_id: "test-track".to_string(),
            samples: Samples::PCM(
                chunk
                    .chunks(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect(),
            ),
            timestamp: (i * CHUNK_SIZE) as u32,
            sample_rate: 16000,
        };

        if let Err(e) = _asr_processor.process_frame(&mut frame) {
            println!("Error processing frame: {}", e);
        }
    }

    // Wait a moment for ASR processing to complete
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    println!("Example completed successfully.");

    Ok(())
}
