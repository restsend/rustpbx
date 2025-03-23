# WebRTC Audio Demo

This is a simple WebRTC audio demo using Axum and the WebRTC crate. It demonstrates how to:

1. Set up an Axum web server
2. Create a WebRTC connection
3. Stream audio from a WAV file to a browser client

## Features

- WebRTC audio streaming
- Browser-based client interface
- WAV file playback over WebRTC
- Simple signaling protocol

## Requirements

- Rust 1.70+
- Web browser with WebRTC support (Chrome, Firefox, Safari, etc.)

## Running the Demo

1. Navigate to the example directory:

```bash
cd examples/webrtc-demo
```

2. Build and run the demo:

```bash
cargo run
```

3. Open your browser and navigate to:

```
http://localhost:3000
```

4. Click the "Start Audio Connection" button to begin streaming.

## Implementation Details

The demo consists of:

- **Frontend**: Simple HTML and JavaScript that establishes a WebRTC connection
- **Backend**: Axum server that handles WebRTC signaling and streams audio

The server reads a WAV file and streams it to connected clients using WebRTC's audio track.

## License

This demo is provided under the same license as the main project. 