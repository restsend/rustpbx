# Tencent Cloud Speech Service Integration

This document provides instructions on how to use the Tencent Cloud ASR (Automatic Speech Recognition) and TTS (Text-to-Speech) integration in RustPBX.

## Prerequisites

Before using the Tencent Cloud speech services, you need to:

1. Create a Tencent Cloud account at [cloud.tencent.com](https://cloud.tencent.com/)
2. Enable the ASR and TTS services in the Tencent Cloud console
3. Obtain your credentials:
   - AppID (Project ID)
   - SecretID
   - SecretKey

## Configuration

### ASR Configuration

To use the Tencent Cloud ASR service, configure the `AsrConfig` with your credentials:

```rust
let asr_config = AsrConfig {
    enabled: true,
    model: None,
    language: Some("zh-CN".to_string()),
    appid: Some("your_app_id".to_string()),
    secret_id: Some("your_secret_id".to_string()),
    secret_key: Some("your_secret_key".to_string()),
    engine_type: Some("16k_zh".to_string()), // Chinese 16kHz model
};
```

Available engine types:
- `16k_zh`: Chinese, 16kHz
- `16k_en`: English, 16kHz
- `16k_ca`: Cantonese, 16kHz
- `16k_ja`: Japanese, 16kHz
- `16k_ko`: Korean, 16kHz

### TTS Configuration

To use the Tencent Cloud TTS service, configure the `TtsConfig` with your credentials:

```rust
let tts_config = TtsConfig {
    url: "".to_string(), // Not used with TencentCloud client
    voice: Some("1".to_string()), // Voice type
    rate: Some(1.0),              // Normal speed
    appid: Some("your_app_id".to_string()),
    secret_id: Some("your_secret_id".to_string()),
    secret_key: Some("your_secret_key".to_string()),
    volume: Some(5),              // Volume level (0-10)
    speaker: Some(1),             // Speaker type
    codec: Some("pcm".to_string()), // PCM format
};
```

Voice types:
- For Chinese: 1-10 for different voices
- For English: 101-107 for different voices

## Using ASR

To transcribe audio with Tencent Cloud ASR:

```rust
// Create ASR client and processor
let asr_client = Arc::new(TencentCloudAsrClient::new());
let (event_sender, mut event_receiver) = broadcast::channel::<AsrEvent>(10);
let asr_processor = AsrProcessor::new(asr_config, asr_client, event_sender);

// Listen for ASR events
tokio::spawn(async move {
    while let Ok(event) = event_receiver.recv().await {
        println!("Transcription: {}", event.text);
    }
});

// Process audio frames
asr_processor.process_frame(&mut audio_frame)?;
```

## Using TTS

To synthesize speech with Tencent Cloud TTS:

```rust
// Create TTS client and processor
let tts_client = Arc::new(TencentCloudTtsClient::new());
let (event_sender, _) = broadcast::channel::<TtsEvent>(10);
let tts_processor = TtsProcessor::new(tts_config, tts_client, event_sender);

// Synthesize text to speech
let text = "欢迎使用腾讯云语音服务";
let audio_data = tts_processor.synthesize(text).await?;

// Use the audio data (e.g., save to file, play, etc.)
std::fs::write("output.pcm", &audio_data)?;
```

## Running Tests

The integration tests for Tencent Cloud services will be skipped if no credentials are provided. To run these tests, you need to:

1. Copy the `.env.example` file to `.env` in the project root:
   ```
   cp .env.example .env
   ```

2. Edit the `.env` file with your Tencent Cloud credentials:
   ```
   TENCENT_APPID=your_app_id
   TENCENT_SECRET_ID=your_secret_id
   TENCENT_SECRET_KEY=your_secret_key
   ```

3. Run the tests:
   ```
   cargo test
   ```

The tests will automatically read credentials from the `.env` file. If the credentials are not found or are empty, the tests requiring real Tencent Cloud API access will be skipped.

## Examples

See the `examples/tencent_asr_tts_example.rs` file for a complete example of using both ASR and TTS services.

## Error Handling

Both ASR and TTS clients will return detailed error messages in case of API failures. Common errors include:

- Missing or invalid credentials
- API rate limits exceeded
- Unsupported audio format or language
- Network connectivity issues

Make sure to implement appropriate error handling in your application.

## Additional Resources

- [Tencent Cloud ASR Documentation](https://cloud.tencent.com/document/product/1093)
- [Tencent Cloud TTS Documentation](https://cloud.tencent.com/document/product/1073)
- [Tencent Cloud SDK GitHub Repository](https://github.com/TencentCloud/tencentcloud-speech-sdk-go) 