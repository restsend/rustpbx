# RustPBX Media Processing System

A robust media processing system for handling WebRTC/RTP audio streams, with support for various audio codecs, voice activity detection, noise reduction, and speech recognition.

## Features

- **Audio Codecs**
  - G.711 μ-law (PCMU)
  - G.711 A-law (PCMA)
  - G.722

- **Voice Activity Detection (VAD)**
  - WebRTC VAD implementation
  - Voice Activity Detector implementation
  - Configurable VAD type selection

- **Noise Reduction**
  - RNNoise-based noise suppression
  - Frame-based processing
  - Support for different sample rates and channels

- **Speech Recognition**
  - QCloud ASR integration
  - Real-time transcription
  - Support for word-level and segment-level results

- **Media Stream Management**
  - Track-based architecture
  - Event-driven design
  - Support for multiple concurrent tracks
  - Recording capability

## Requirements

- Rust 1.75 or later
- Cargo package manager

## Installation

Add the following to your `Cargo.toml`:

```toml
[dependencies]
rustpbx = "0.1"
```

## Usage

### Basic Media Stream Setup

```rust
use rustpbx::media::{
    MediaStream,
    MediaStreamBuilder,
    track::Track,
};

#[tokio::main]
async fn main() {
    // Create a media stream
    let stream = MediaStreamBuilder::new()
        .event_buf_size(32)
        .build();

    // Subscribe to stream events
    let mut events = stream.subscribe();

    // Start serving the stream
    tokio::spawn(async move {
        stream.serve().await.unwrap();
    });

    // Handle events
    while let Ok(event) = events.recv().await {
        match event {
            MediaStreamEvent::StartSpeaking(track_id, ssrc) => {
                println!("Track {} (SSRC: {}) started speaking", track_id, ssrc);
            }
            MediaStreamEvent::Silence(track_id, ssrc) => {
                println!("Track {} (SSRC: {}) is silent", track_id, ssrc);
            }
            MediaStreamEvent::Transcription(track_id, ssrc, text) => {
                println!("Track {} (SSRC: {}): {}", track_id, ssrc, text);
            }
            _ => {}
        }
    }
}
```

### Adding Processors to a Track

```rust
use rustpbx::media::{
    noise::NoiseReducer,
    vad::{VadProcessor, VadType},
    asr::qcloud::{QCloudAsr, QCloudAsrConfig},
};

// Create processors
let noise_reducer = NoiseReducer::new();
let vad = VadProcessor::new(track_id.clone(), ssrc, VadType::WebRTC, event_sender.clone());
let asr = QCloudAsr::new(
    track_id.clone(),
    ssrc,
    QCloudAsrConfig::default(),
    event_sender.clone(),
);

// Add processors to track
track.with_processors(vec![
    Box::new(noise_reducer),
    Box::new(vad),
    Box::new(asr),
]);
```

## Testing

Run the test suite:

```bash
cargo test
```

Run specific test categories:

```bash
cargo test --test codecs    # Run codec tests
cargo test --test vad       # Run VAD tests
cargo test --test noise     # Run noise reduction tests
```

## License

MIT License
