use anyhow::Result;
use dotenv::dotenv;
use rustpbx::synthesis::{TencentCloudTtsClient, TtsClient, TtsConfig};
use rustpbx::transcription::{AsrClient, AsrConfig, TencentCloudAsrClient};
use std::fs::File;
use std::io::Write;

// Helper function to get credentials from .env
fn get_tencent_cloud_credentials() -> Option<(String, String, String)> {
    // Load .env file if it exists
    let _ = dotenv();

    // Try to get the credentials from environment variables
    let secret_id = std::env::var("TENCENT_SECRET_ID").ok()?;
    let secret_key = std::env::var("TENCENT_SECRET_KEY").ok()?;
    let appid = std::env::var("TENCENT_APPID").ok()?;

    // Return None if any of the credentials are empty
    if secret_id.is_empty() || secret_key.is_empty() || appid.is_empty() {
        return None;
    }

    Some((secret_id, secret_key, appid))
}

#[tokio::test]
async fn test_tts_asr_integration() -> Result<()> {
    // Get credentials
    let credentials =
        get_tencent_cloud_credentials().expect("Missing Tencent Cloud credentials in .env file");
    let (secret_id, secret_key, appid) = credentials;

    // Create TTS client and config
    let tts_client = TencentCloudTtsClient::new();
    let tts_config = TtsConfig {
        url: "".to_string(),          // Not used for TencentCloud
        voice: Some("1".to_string()), // 标准女声
        rate: Some(1.0),
        appid: Some(appid.clone()),
        secret_id: Some(secret_id.clone()),
        secret_key: Some(secret_key.clone()),
        volume: Some(0),
        speaker: Some(1),
        codec: Some("pcm".to_string()),
    };

    // Test text to synthesize
    let test_text = "这是一个测试音频文件";
    println!("Synthesizing text: {}", test_text);

    // Generate audio using TTS
    let audio_data = tts_client.synthesize(test_text, &tts_config).await?;
    println!("Generated {} bytes of audio data", audio_data.len());

    // Save the audio data to a temporary file
    let temp_file = "test_audio.pcm";
    let mut file = File::create(temp_file)?;
    file.write_all(&audio_data)?;
    println!("Saved audio data to {}", temp_file);

    // Convert bytes to i16 samples
    let samples: Vec<i16> = audio_data
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    // Create ASR client and config
    let asr_client = TencentCloudAsrClient::new();
    let asr_config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        engine_type: Some("16k_zh".to_string()),
    };

    // Transcribe the audio
    println!("Transcribing audio...");
    let transcription = asr_client.transcribe(&samples, 16000, &asr_config).await?;
    println!("Transcription result: {}", transcription);

    // Clean up the temporary file
    std::fs::remove_file(temp_file)?;

    // Verify the transcription contains the test text (allowing for some variation)
    assert!(
        transcription.contains("测试") || transcription.contains("音频"),
        "Transcription '{}' should contain parts of the original text '{}'",
        transcription,
        test_text
    );

    Ok(())
}
