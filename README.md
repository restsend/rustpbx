# RustPBX - AI-Powered Software-Defined PBX

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/restsend/rustpbx)

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

## 🐳 Docker Deployment

### Quick Start with Docker

1. **Pull the Docker image:**
```bash
docker pull ghcr.io/restsend/rustpbx:latest
```

2. **Create environment configuration:**
```bash
# Create .env file
cat > .env << EOF
# Tencent Cloud ASR/TTS Configuration
TENCENT_APPID=your_tencent_app_id
TENCENT_SECRET_ID=your_tencent_secret_id
TENCENT_SECRET_KEY=your_tencent_secret_key
EOF
```

3. **Create config.toml:**
```bash
# Create config.toml
cat > config.toml << EOF
http_addr = "0.0.0.0:8080"
log_level = "info"
stun_server = "stun.l.google.com:19302"
recorder_path = "/tmp/recorders"
media_cache_path = "/tmp/mediacache"

[ua]
addr="0.0.0.0"
udp_port=13050

[proxy]
modules = ["acl", "auth", "registrar", "call"]
addr = "0.0.0.0"
udp_port = 15060
registrar_expires = 60
ws_handler= "/ws"

# ACL rules
acl_rules = [
    "allow 10.0.0.0/8",
    "allow all",
]

[proxy.media_proxy]
mode = "auto"
rtp_start_port = 20000
rtp_end_port = 30000

[proxy.user_backend]
type = "memory"
users = [
    { username = "bob", password = "123456", realm = "127.0.0.1" },
    { username = "alice", password = "123456", realm = "127.0.0.1" },
]

[callrecord]
type = "local"
root = "/tmp/cdr"
EOF
```

4. **Run with Docker:**
```bash
docker run -d \
  --name rustpbx \
  -p 8080:8080 \
  -p 15060:15060/udp \
  -p 13050:13050/udp \
  -p 20000-30000:20000-30000/udp \
  --env-file .env \
  -v $(pwd)/config.toml:/app/config.toml \
  -v $(pwd)/recordings:/tmp/recorders \
  docker.io/library/rustpbx:latest \
  --conf /app/config.toml
```

5. **Access the service:**
- Web Interface: http://localhost:8080
- SIP Proxy: localhost:15060
- User Agent: localhost:13050

### Environment Variables

The following environment variables are required for Tencent Cloud ASR/TTS services:

| Variable | Description | Required |
|----------|-------------|----------|
| `TENCENT_APPID` | Your Tencent Cloud App ID | Yes |
| `TENCENT_SECRET_ID` | Your Tencent Cloud Secret ID | Yes |
| `TENCENT_SECRET_KEY` | Your Tencent Cloud Secret Key | Yes |

### Configuration Options

Key configuration options in `config.toml`:

- **HTTP Server**: `http_addr` - Web interface and API endpoint
- **SIP Proxy**: `proxy.udp_port` - SIP proxy server port
- **User Agent**: `ua.udp_port` - Outbound call user agent port
- **Media Proxy**: `proxy.media_proxy` - RTP port range for media proxying
- **Call Recording**: `callrecord.root` - Directory for call recordings

## 🧪 Go Client Integration

### Using rustpbxgo Client Library

See `https://github.com/restsend/rustpbxgo`

### API Documentation

#### SIP Workflow
![Sip](./docs/sip.png)

The SIP workflow demonstrates how external applications can initiate calls through RustPBX, leveraging the full SIP protocol stack for reliable voice communications.

#### WebRTC Workflow
![Webrtc](./docs/webrtc.png)

The WebRTC workflow shows how web applications can establish direct peer-to-peer connections via RustPBX, enabling modern browser-based voice applications.

For detailed API documentation, see [API Documentation](./docs/api.md).

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

- [API Reference](./docs/api.md) - Complete REST API documentation
- [Architecture Diagrams](docs/) - System architecture and workflows

## 🤝 Contributing

This project is currently in active development. We welcome contributions and feedback from the community.

## 📄 License

MIT License - see [LICENSE](LICENSE) file for details.

## 🏗 Project Status

**Work in Progress** - Core functionality is implemented and being actively refined. The system is suitable for development and testing environments.
