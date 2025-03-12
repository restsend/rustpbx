# RustPBX

A SIP PBX implementation in Rust.

## Features

- **Media Stream Processing**: Process WebRTC and RTP media streams with support for:
  - DTMF detection
  - Voice Activity Detection (VAD)
  - Noise reduction
  - PCM recording
  - Multiple input/output formats (WebRTC, RTP, WAV, PCM, TTS)

## Usage

### Media Stream Example

```rust
use rustpbx::media::{
    MediaStream, MediaStreamConfig, MediaSourceConfig, MediaSinkConfig,
    EventType, EventData, source::Codec,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a configuration for the media stream
    let config = MediaStreamConfig {
        input: MediaSourceConfig::RTP {
            bind_address: "127.0.0.1:5000".parse()?,
            codec: Codec::Opus,
        },
        output: MediaSinkConfig::WAVFile {
            path: "output.wav".to_string(),
            sample_rate: 48000,
            channels: 1,
        },
        record_pcm: Some(rustpbx::media::PCMRecorderConfig {
            path: "recording.pcm".to_string(),
        }),
        enable_vad: true,
        enable_dtmf_detection: true,
    };
    
    // Create the media stream
    let mut stream = MediaStream::new(config)?;
    
    // Register event handlers
    stream.on(EventType::DTMFDetected, Box::new(|event| {
        if let EventData::DTMF { digit, .. } = event.data {
            println!("DTMF detected: {}", digit);
        }
    }));
    
    // Start the media stream
    stream.start().await?;
    
    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    
    // Stop the media stream
    stream.stop().await?;
    
    Ok(())
}
```

## Building and Testing

```bash
# Build the project
cargo build

# Run tests
cargo test

# Run the media stream example
cargo run --example media_stream_example
```

## License

This project is licensed under the MIT License - see the LICENSE file for details.
