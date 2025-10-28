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
- **Speech-to-Text (ASR)**: Real-time speech recognition with multiple providers (Tencent Cloud, Aliyun, Deepgram)
- **Text-to-Speech (TTS)**: High-quality speech synthesis with emotion and speaker control
- **LLM Integration**: OpenAI-compatible LLM proxy for intelligent conversation handling
- **Voice Activity Detection**: WebRTC and Silero VAD and Ten VAD support for optimal speech processing
- **Noise Suppression**: Real-time audio denoising(rnnoise) for crystal-clear conversations

### RESTful API & WebSocket
- **RESTful Endpoints**: Complete REST API for call management and control
- **WebSocket Commands**: Real-time call control via WebSocket connections
- **Call Management**: List, monitor, and control active calls

## 🛠 Quick Start

### Prerequisites
- Rust 1.75 or later
- Cargo package manager
- opus, alsa
- pkg-config

Linux: 
```bash
apt-get install -y libasound2-dev libopus-dev
```

Mac:
```bash
brew install opus
```

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
2. **Create config.toml:**
> Proxy and UserAgent both worked.
>

```bash
# Create config.toml
cat > config.toml << EOF
http_addr = "0.0.0.0:8080"
log_level = "info"
recorder_path = "/tmp/recorders"
media_cache_path = "/tmp/mediacache"
database_url = "sqlite://./db/rustpbx.sqlite3"

[ua]
addr="0.0.0.0"
udp_port=13050

[console]
#session_secret = "please_change_me_to_a_random_secret"
base_path = "/console"

[proxy]
modules = ["acl", "auth", "registrar", "call"]
addr = "0.0.0.0"
udp_port = 15060
registrar_expires = 60
ws_handler= "/ws"
media_proxy = "auto"

# ACL rules
acl_rules = [
    "allow 10.0.0.0/8",
    "allow all",
]

[[proxy.user_backends]]
type = "memory"
users = [
    { username = "bob", password = "123456" },
    { username = "alice", password = "123456" },
]

[[proxy.user_backends]]
type = "extension"

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
  -v $(pwd)/db:/app/db \
  -v $(pwd)/config.toml:/app/config.toml \
  -v $(pwd)/recordings:/tmp/recorders \
  ghcr.io/restsend/rustpbx:latest \
  --conf /app/config.toml
```
 - Create super user via cli(**optional**)
```bash
docker exec rustpbx /app/rustpbx --conf /app/config.toml --super-user=YOUR --super-password=PASS
```

1. **Access the service:**
- Web Interface: http://localhost:8080/console/
  - Login via `YOUR` + `PASS`
- SIP Proxy: localhost:15060
- User Agent: localhost:13050


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
- [VoiceAgent Integration with Telephony Networks](./docs/how%20webrtc%20work%20with%20sip(en).md)
- [VoiceAgent 与电话网络互通的技术实现](./docs/how%20webrtc%20work%20with%20sip(zh).md)


## 🤝 Contributing

This project is currently in active development. We welcome contributions and feedback from the community.

## 📄 License

MIT License - see [LICENSE](LICENSE) file for details.

## 🏗 Project Status

**Work in Progress** - Core functionality is implemented and being actively refined. The system is suitable for development and testing environments.
