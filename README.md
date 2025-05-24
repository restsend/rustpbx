# RustPBX - AI-Powered Software-Defined PBX

RustPBX is a high-performance, secure software-defined PBX (Private Branch Exchange) system implemented in Rust, designed to support AI-powered communication pipelines and modern voice applications.

## 🚀 Key Features

### SIP PBX Core
- **Full SIP Stack**: Complete SIP proxy server with registration, authentication, and call routing
- **Media Proxy**: Advanced RTP/RTCP media proxying with NAT traversal support
- **Multi-Transport**: UDP, TCP, and WebSocket transport support
- **Call Recording**: Built-in call recording with multiple storage backends
- **User Management**: Flexible user authentication and registration system

### AI Voice Agent Services
- **Speech-to-Text (ASR)**: Real-time speech recognition with multiple providers (Tencent Cloud, VoiceAPI)
- **Text-to-Speech (TTS)**: High-quality speech synthesis with emotion and speaker control
- **LLM Integration**: OpenAI-compatible LLM proxy for intelligent conversation handling
- **Voice Activity Detection**: WebRTC and Silero VAD support for optimal speech processing
- **Noise Suppression**: Real-time audio denoising for crystal-clear conversations

### WebRTC Integration
- **Direct WebRTC Calls**: Native WebRTC support for web-based communications
- **STUN/TURN Support**: Built-in ICE server management for NAT traversal
- **Codec Support**: Multiple audio codecs (PCMU, PCMA, G.722, PCM)
- **Real-time Media**: Low-latency audio streaming and processing

### RESTful API & WebSocket
- **RESTful Endpoints**: Complete REST API for call management and control
- **WebSocket Commands**: Real-time call control via WebSocket connections
- **Call Management**: List, monitor, and control active calls
- **LLM Proxy**: Built-in proxy for AI language model services

## 📊 Architecture

### SIP Workflow
![Sip](./docs/sip.png)

The SIP workflow demonstrates how external applications can initiate calls through RustPBX, leveraging the full SIP protocol stack for reliable voice communications.

### WebRTC Workflow
![Webrtc](./docs/webrtc.png)

The WebRTC workflow shows how web applications can establish direct peer-to-peer connections via RustPBX, enabling modern browser-based voice applications.

## 🛠 Quick Start

### Prerequisites
- Rust 1.75 or later
- Cargo package manager

### Installation
```bash
git clone https://github.com/restsend/rustpbx
cd rustpbx
cargo build --release
```

### Configuration
```bash
cp config.toml.example config.toml
# Edit config.toml with your settings
```

### Running
```bash
cargo run --bin rustpbx -- --conf config.toml
```

### Web Interface
Access the web interface at `http://localhost:8080` to test voice agent features and manage calls.

## 🎯 Use Cases

### AI Customer Service
Build intelligent customer service systems with automated speech recognition, natural language processing, and synthetic voice responses.

### Voice Assistant Applications
Create voice-controlled applications with real-time speech processing and AI-powered conversation handling.

### WebRTC Contact Centers
Deploy browser-based contact center solutions with advanced call routing and AI assistance.

### Telephony Integration
Integrate traditional SIP phones and systems with modern AI voice processing capabilities.

## 📋 Core Components

- **SIP Proxy Server** (`proxy/`): Full-featured SIP server with modular architecture
- **User Agent** (`useragent/`): SIP user agent for outbound calls
- **Media Engine** (`media/`): Audio processing pipeline with codec support
- **Voice Synthesis** (`synthesis/`): TTS engines for multiple providers
- **Speech Recognition** (`transcription/`): ASR engines with streaming support
- **LLM Integration** (`llm/`): Language model proxy and integration
- **Call Recording** (`callrecord/`): Call recording and storage management
- **RESTful API** (`handler/`): HTTP API and WebSocket endpoints

## 🔧 Configuration Features

### SIP Proxy
- Modular proxy architecture with pluggable modules
- User authentication and registration
- Call routing and forwarding
- CDR (Call Detail Records) generation

### Media Proxy
- Automatic NAT detection and media proxying
- Configurable RTP port ranges
- Support for multiple codecs
- Real-time media relay

### AI Services
- Multiple ASR/TTS provider support
- Configurable LLM endpoints
- Voice activity detection
- Audio preprocessing and enhancement

## 📚 Documentation

- [API Reference](./docs/API.md) - Complete REST API documentation
- [Media Proxy Guide](./docs/MEDIAPROXY_README.md) - Detailed media proxy configuration
- [Architecture Diagrams](docs/) - System architecture and workflows

## 🤝 Contributing

This project is currently in active development. We welcome contributions and feedback from the community.

## 📄 License

MIT License - see [LICENSE](LICENSE) file for details.

## 🏗 Project Status

**Work in Progress** - Core functionality is implemented and being actively refined. The system is suitable for development and testing environments.
