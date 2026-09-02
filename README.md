# RustPBX

![Crates.io License](https://img.shields.io/crates/l/rustpbx)
![GitHub commit activity](https://img.shields.io/github/commit-activity/m/restsend/rustpbx) ![Crates.io Total Downloads](https://img.shields.io/crates/d/rustpbx) ![GitHub Repo stars](https://img.shields.io/github/stars/restsend/rustpbx)

**A high-performance, software-defined PBX built in Rust.**

All call control is externalized via HTTP/WebSocket/Webhook — routing, media control, and events are programmable in any language. No C modules, no recompilation.

**[GitHub](https://github.com/restsend/rustpbx)** | **[Website](https://miuda.ai)**

---

## Quick Start

### Option 1: Docker (recommended)

```bash
mkdir -p rustpbx && cd rustpbx
curl -O https://raw.githubusercontent.com/restsend/rustpbx/main/config.toml.example

docker run -d --name rustpbx --net host \
  -v $(pwd)/config.toml.example:/app/config.toml \
  ghcr.io/restsend/rustpbx:latest --conf /app/config.toml

# Create the admin account (first time only)
docker exec rustpbx /app/rustpbx --conf /app/config.toml \
  --super-username admin --super-password changeme
```

### Option 2: Build from source

```bash
git clone --recurse-submodules https://github.com/restsend/rustpbx
cd rustpbx
cargo run --release -- --conf config.toml.example
```

That's it — the example config boots a full PBX out of the box:

| Access | Value |
|---|---|
| Web Console | http://localhost:8080/console/ |
| SIP Proxy | `udp://localhost:15060` |
| Test extensions | `bob` / `alice`, password `123456` |

> No config at all? `cargo run --release` also works — built-in defaults: HTTP `0.0.0.0:8080`, SQLite, SIP UDP `5060`.

### Minimal config

Want your own `config.toml`? This is all you need:

```toml
http_addr = "0.0.0.0:8080"
database_url = "sqlite://rustpbx.sqlite3"

[proxy]
addr = "0.0.0.0"
udp_port = 5060
modules = ["auth", "registrar", "call"]

[[proxy.user_backends]]
type = "memory"
users = [{ username = "1001", password = "password" }]

[console]
base_path = "/console"
```

---

## Features

| | |
|---|---|
| **SIP & Media** | Full SIP stack (UDP/TCP/WS/TLS/WebRTC), RTP relay & transcoding, NAT traversal, TLS/SRTP with auto ACME certs |
| **Routing & Control** | HTTP Router (webhook routing decisions), RWI WebSocket (real-time call control), Queue/ACD |
| **AI-Native** | AI agents as native call participants — listen, speak, barge, transfer via WebSocket |
| **Recording & Analytics** | SipFlow unified SIP+RTP capture, offline transcription (SenseVoice), CDR webhooks |
| **Operations** | Built-in Web Console & WebRTC phone, RBAC, Prometheus metrics, OpenTelemetry |

Performance: 5000 concurrent calls with RTP relay on ~3.8 cores, 0% packet loss — linear scaling. See [benchmark details](tests/bench/bench.md).

---

## Programmable Interfaces

### HTTP Router

Every incoming INVITE calls your webhook. Return JSON to decide routing:

```json
// POST to your webhook: { "call_id": "abc-123", "from": "sip:+861390000@trunk", "to": "sip:400800" }
// Your response:       { "action": "forward", "targets": ["sip:1001@internal"], "record": true }
```

Actions: `forward` · `reject` · `abort` · `spam`

### RWI (Real-time WebSocket Interface)

JSON-over-WebSocket for in-call control: `originate`, `answer`, `hangup`, `bridge`, `transfer`, `play`, `record.start`, queue/ACD commands, supervisor `listen`/`whisper`/`barge`, conference commands, and more.

---

## Documentation

| Guide | Description |
|---|---|
| [Configuration Guide](docs/configuration.md) | All config options |
| [API Integration Guide](docs/api_integration_guide.md) | HTTP Router, Webhooks, Call Control |
| [RWI Protocol](docs/rwi.md) | WebSocket Interface |
| [Outbound Dial SSE API](docs/outbound_dial_api.md) | Predictive outbound dialing |
| [Live Transcript SSE API](docs/live_transcript_api.md) | Real-time transcription |

## Troubleshooting

SIP 401 behind NAT/Docker — set the realm explicitly:

```toml
[proxy]
realms = ["your-public-ip:5060"]
```

## License

Community: MIT · Commercial: [hi@miuda.ai](mailto:hi@miuda.ai) · **https://miuda.ai**
