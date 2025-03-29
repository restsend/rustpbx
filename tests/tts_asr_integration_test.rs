use anyhow::Result;
use dotenv::dotenv;
use rustpbx::{
    media::track::file::read_wav_file,
    transcription::{TencentCloudAsrClient, TranscriptionClient, TranscriptionConfig},
};
use tracing::{error, info};

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
async fn test_asr_integration() -> Result<()> {
    // Initialize tracing for better debug output
    let _ = tracing_subscriber::fmt::try_init();

    // Get credentials
    let credentials =
        get_tencent_cloud_credentials().expect("Missing Tencent Cloud credentials in .env file");
    let (secret_id, secret_key, appid) = credentials;
    println!(
        "Using Tencent Cloud credentials with SecretID: {}",
        secret_id
    );

    // Path to the test audio file
    let wav_path = "fixtures/hello_book_course_zh_16k.wav";
    info!("Using test audio file: {}", wav_path);

    // Read the audio file
    let (samples, _) = read_wav_file(wav_path)?;
    info!("Loaded {} samples from audio file", samples.len());

    // Create transcription client and config
    let transcription_client = TencentCloudAsrClient::new();
    let transcription_config = TranscriptionConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()), // Required in struct but not used by TencentCloud ASR
        appid: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        engine_type: Some("16k_zh".to_string()), // This is what controls the language for TencentCloud
    };

    // Transcribe the audio
    info!("Transcribing audio...");

    // Try to transcribe and handle any errors
    match transcription_client
        .transcribe(&samples, 16000, &transcription_config)
        .await
    {
        Ok(transcription_result) => {
            // Verify we got a result
            match transcription_result {
                Some(transcription) => {
                    info!("Transcription result: {}", transcription);

                    // Verify the transcription contains parts of the expected text
                    assert!(
                            transcription.contains("您好") || transcription.contains("预约") || transcription.contains("课程"),
                            "Transcription '{}' should contain parts of the expected text '您好,请帮我预约课程'",
                            transcription
                        );
                }
                None => {
                    error!("Transcription returned None");
                    assert!(false, "Transcription should not return None");
                }
            }
        }
        Err(e) => {
            error!("Error during transcription: {:?}", e);
            panic!("Transcription failed: {:?}", e);
        }
    }

    Ok(())
}
