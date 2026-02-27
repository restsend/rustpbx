# RustPBX

![Crates.io License](https://img.shields.io/crates/l/rustpbx)
 ![GitHub commit activity](https://img.shields.io/github/commit-activity/m/restsend/rustpbx) ![Crates.io Total Downloads](https://img.shields.io/crates/d/rustpbx) ![GitHub Repo stars](https://img.shields.io/github/stars/restsend/rustpbx)


A high-performance Software-Defined PBX built in Rust. Designed as a programmable foundation for **AI-powered call centers** — route, record, transcribe, and webhook everything.

Different from Asterisk/FreeSWITCH (C modules), RustPBX exposes all call control via **HTTP/Webhook**, so any language can drive call logic.

> **Note**: The Voice Agent functionality has been moved to a separate repository: [Active Call](https://github.com/restsend/active-call). This repository now focuses on the SIP Proxy and PBX features.
---

## Editions

| | Community | Commerce |
|---|---|---|
| License | MIT | Commercial |
| SIP Proxy + Media | ✅ | ✅ |
| Queue / ACD | ✅ | ✅ |
| HTTP Router (dynamic routing) | ✅ | ✅ |
| Call Recording + SipFlow | ✅ | ✅ |
| Transcript (SenseVoice offline) | ✅ | ✅ |
| Web Console | ✅ | ✅ |
| **Voip Wholesale** (VOS3000 alternative) | ❌ | ✅ |
| **Voicemail Pro** | ❌ | ✅ |
| **Enterprise Auth** (LDAP/SAML/MFA) | ❌ | ✅ |
| **Endpoint Manager** (phone auto-provisioning) | ❌ | ✅ |

---

## Core Capabilities

- **SIP Proxy** — Full SIP stack (UDP/TCP/WebSocket), registration, auth, B2BUA
- **HTTP Router** — Every INVITE hits your webhook; you return routing decision in JSON
- **Queue / ACD** — Sequential or parallel agent ringing, hold music, fallback actions
- **Media Proxy** — RTP relay, NAT traversal, WebRTC ↔ SIP bridging
- **SipFlow Recording** — Unified SIP+RTP capture; date-organized, query-on-demand (no file-handle exhaustion)
- **Transcript** — Post-call transcription via local SenseVoice (offline, no cloud dependency)
- **CDR Webhooks** — Push call detail records + recordings to your system on hangup
- **WebRTC Phone** — Built-in browser softphone for testing

---

## Quick Start (Docker)

```bash
# Commerce image (includes Wholesale + all commercial plugins)
docker pull docker.cnb.cool/miuda.ai/rustpbx:latest

# Community image
docker pull ghcr.io/restsend/rustpbx:latest
```

**Minimal `config.toml`**:
```toml
http_addr = "0.0.0.0:8080"
database_url = "sqlite://rustpbx.sqlite3"

[console]
base_path = "/console"
allow_registration = false

[proxy]
addr = "0.0.0.0"
udp_port = 5060
modules = ["auth", "registrar", "call"]

[[proxy.user_backends]]
type = "memory"
users = [{ username = "1001", password = "password" }]

[sipflow]
type = "local"
root = "./config/cdr"
subdirs = "hourly"
```

```bash
docker run -d --name rustpbx --net host \
  -v $(pwd)/config.toml:/app/config.toml \
  -v $(pwd)/config:/app/config \
  ghcr.io/restsend/rustpbx:latest --conf /app/config.toml

# Create first admin
docker exec rustpbx /app/rustpbx --conf /app/config.toml \
  --super-username admin --super-password changeme
```

Web console: `http://localhost:8080/console/`  
SIP proxy: `udp://localhost:5060`

---

## Build from Source

**Dependencies** (Linux):
```bash
apt-get install -y cmake pkg-config libasound2-dev libssl-dev libopus-dev
```

macOS:
```bash
brew install cmake openssl pkg-config
```

```bash
git clone https://github.com/restsend/rustpbx
cd rustpbx
cargo build --release
cargo run --bin rustpbx -- --conf config.toml.example
```

Cross-compilation for `aarch64` / `x86_64` via [cross](https://github.com/cross-rs/cross):
```bash
cargo install cross
cross build --release --target aarch64-unknown-linux-gnu
```

---

## HTTP Router — The Key Extension Point

RustPBX calls your API on every incoming INVITE. You decide what happens:

```toml
[proxy.http_router]
url = "https://your-api.com/route"
timeout_ms = 3000
```

```json
// POST to your endpoint:
{ "call_id": "abc-123", "from": "sip:+861390000@trunk", "to": "sip:400800", "direction": "inbound" }

// Your response:
{ "action": "forward", "targets": ["sip:ai-agent@internal"], "record": true }
```

Actions: `forward` · `reject` · `abort` · `spam`  
See [API Integration Guide](docs/api_integration_guide.md) for the full webhook and active call control reference.

---

## Screenshots

| Extensions | Call Records | Route Editor |
|---|---|---|
| ![](./docs/screenshots/extensions.png) | ![](./docs/screenshots/call-logs.png) | ![](./docs/screenshots/route-editor.png) |

| Transcript | SIP Flow | WebRTC Phone |
|---|---|---|
| ![](./docs/screenshots/call-detail-transcript.png) | ![](./docs/screenshots/call-detail-sipflow.png) | ![](./docs/screenshots/web-dailer.png) |

---

## Documentation

| | |
|---|---|
| [Configuration Guide](docs/configuration.md) | All config options |
| [API Integration Guide](docs/api_integration_guide.md) | HTTP Router, Webhooks, Active Call Control |
| [Product Roadmap](PRODUCT_ROADMAP.md) | Commercial plugins & Q2 2026 plan |

---

## Troubleshooting

**SIP 401 behind NAT/Docker** — set the realm explicitly:
```toml
[proxy]
realms = ["your-public-ip:5060"]
```

---

## License

Community edition: MIT  
Commercial edition : [hi@miuda.ai](mailto:hi@miuda.ai)
