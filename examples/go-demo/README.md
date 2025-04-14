# RustPBX Go Demo

This is a Go demo for the RustPBX server that demonstrates how to:

1. Connect to the RustPBX server via WebSocket
2. Establish a WebRTC call
3. Capture audio from the local microphone using malgo
4. Encode audio to G.722 using go722
5. Send and receive audio via WebRTC
6. Generate text using OpenAI's LLM
7. Send TTS commands to the server

## Prerequisites

- Go 1.24 or later
- A microphone for audio capture
- An OpenAI API key for LLM functionality

## Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/rustpbx.git
cd rustpbx/examples/go-demo

# Install dependencies
go mod tidy
```

## Usage

```bash
# Run the demo with default settings
go run .

# Run the demo with custom server URL
go run . -server ws://your-rustpbx-server.com/webrtc

# Run the demo with OpenAI API key
go run . -openai-key your-openai-api-key

# Run the demo with OpenAI API key from environment variable
export OPENAI_API_KEY=your-openai-api-key
go run .
```

## Testing

```bash
# Run all tests
go test -v

# Run a specific test
go test -v -run TestDeviceManager
```

## Project Structure

- `main.go`: Entry point for the application
- `client.go`: WebRTC client implementation
- `call.go`: WebSocket connection and call management
- `devices.go`: Audio device management using malgo
- `main_test.go`: Unit tests

## How It Works

1. The demo connects to the RustPBX server via WebSocket
2. It creates a WebRTC offer and sends it to the server
3. It captures audio from the local microphone using malgo
4. It encodes the audio to G.722 using go722
5. It sends the encoded audio to the server via WebRTC
6. It receives events from the server and handles them
7. It can generate text using OpenAI's LLM and send it as TTS to the server

## License

This project is licensed under the same license as the RustPBX project. 