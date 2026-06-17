# RustPBX

![Crates.io License](https://img.shields.io/crates/l/rustpbx)
 ![GitHub commit activity](https://img.shields.io/github/commit-activity/m/restsend/rustpbx) ![Crates.io Total Downloads](https://img.shields.io/crates/d/rustpbx) ![GitHub Repo stars](https://img.shields.io/github/stars/restsend/rustpbx)

**A high-performance, software-defined PBX built in Rust** — the AI-native communication platform for next-gen contact centers.

RustPBX externalizes all call control via **HTTP/WebSocket/Webhook**. Route decisions, media control, and event streams are fully programmable in any language.

> Voice Agent functionality has moved to [Active Call](https://github.com/restsend/active-call). This repo focuses on SIP Proxy & PBX.

**[GitHub](https://github.com/restsend/rustpbx)** | **[Website](https://miuda.ai)**

---

## Quick Start

Run RustPBX with minimal config in 2 commands:

**Minimal `config.toml`**:
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
allow_registration = false
```

```bash
docker run -d --name rustpbx --net host \
  -v $(pwd)/config.toml:/app/config.toml \
  ghcr.io/restsend/rustpbx:latest --conf /app/config.toml

# Create admin
docker exec rustpbx /app/rustpbx --conf /app/config.toml \
  --super-username admin --super-password changeme
```

| Access | URL |
|--------|-----|
| Web Console | `http://localhost:8080/console/` |
| SIP Proxy | `udp://localhost:5060` |
| Register SIP phone as | `1001` / `password` |

> **Commerce image** (includes Wholesale + all plugins): `docker pull docker.cnb.cool/miuda.ai/rustpbx:latest`

---

## Why RustPBX?

| Software-Defined | AI-Native | High Performance |
|---|---|---|
| Every INVITE calls your **HTTP webhook**. Return JSON routing decisions. No recompilation needed. | AI agents are **native participants** — listen, speak, barge, transfer via WebSocket. | 800 concurrent calls with RTP proxy: **6ms latency, 0% loss, 280MB memory**. |

---

## Core Capabilities

**SIP & Media** — Full SIP stack (UDP/TCP/WS/TLS/WebRTC), RTP relay, NAT traversal, TLS/SRTP with auto ACME certs. Fast registration via JWT or HTTP token (skip 401/407).

**Routing & Control** — HTTP Router (dynamic routing decisions), RWI WebSocket Interface (real-time call control), Queue/ACD (sequential or parallel agent ringing).

**Recording & Analytics** — SipFlow unified SIP+RTP capture, post-call transcript via local SenseVoice (offline), CDR webhooks.

**Operations** — Built-in Web Console, WebRTC Phone, RBAC, Prometheus metrics + OpenTelemetry.

---

## Programmable Interfaces

RustPBX exposes all call logic through standard protocols — no C modules, no recompilation.

### HTTP Router
Every incoming INVITE calls your webhook. Return JSON to decide routing.

```toml
[proxy.http_router]
url = "https://your-api.com/route"
timeout_ms = 3000
```

```json
// POST to your webhook: { "call_id": "abc-123", "from": "sip:+861390000@trunk", "to": "sip:400800" }
// Your response:       { "action": "forward", "targets": ["sip:ai-agent@internal"], "record": true }
```

Actions: `forward` · `reject` · `abort` · `spam`

### RWI (Real-time WebSocket Interface)
JSON-over-WebSocket for in-call control:

| Category | Commands |
|---|---|
| Call Control | `originate`, `answer`, `hangup`, `bridge`, `transfer`, `hold` |
| Media | `play`, `stop`, `stream_start`, `inject_start` (PCM) |
| Recording | `record.start`, `pause`, `resume`, `stop` |
| Queue | `enqueue`, `dequeue`, `assign_agent`, `requeue` |
| Supervisor | `listen`, `whisper`, `barge`, `takeover` |
| Conference | `create`, `add`, `remove`, `mute`, `destroy` |

See [API Integration Guide](docs/api_integration_guide.md) and [RWI Protocol](docs/rwi.md).

---

## Editions

| | Community | Commerce |
|---|---|---|
| License | MIT | Commercial |
| SIP Proxy + Media | ✅ | ✅ |
| HTTP Router | ✅ | ✅ |
| Queue / ACD | ✅ | ✅ |
| Recording + SipFlow | ✅ | ✅ |
| Transcript (offline SenseVoice) | ✅ | ✅ |
| Web Console | ✅ | ✅ |
| RWI | ✅ | ✅ |
| **VoIP Wholesale** (VOS3000 alt) | ❌ | ✅ |
| **IVR Visual Editor** | ❌ | ✅ |
| **Voicemail Pro** | ❌ | ✅ |
| **Enterprise Auth** (LDAP/SAML/MFA) | ❌ | ✅ |
| **Endpoint Manager** (auto-provisioning) | ❌ | ✅ |

---

## Benchmark

Tested on 2026-04-03 · RustPBX 0.4.0 · Linux x86_64 · 16 cores / 32 GB · G.711 PCMU

| Level | Scenario | Completion | Peak | Loss | Latency | CPU | Mem |
|---|---|---|---|---|---|---|---|
| 500 | signaling only | 100% | 500 | 0% | 4.40ms | 32.4% | 137 MB |
| 500 | + RTP proxy | 100% | 500 | 0% | 3.73ms | 98.4% | 183 MB |
| 500 | + sipflow | 100% | 500 | 0% | 5.96ms | 101% | 198 MB |
| 800 | signaling only | 100% | 800 | 0% | 8.32ms | 47.9% | 192 MB |
| 800 | + RTP proxy | 100% | 800 | 0% | 6.38ms | 155% | 265 MB |
| 800 | + sipflow | 100% | 800 | 0% | 6.08ms | 156% | 280 MB |

Per-channel overhead: ~0.06% CPU / 0.24 MB (signaling); ~0.19% CPU / 0.33 MB (with RTP proxy).

> See [Benchmark Details](tests/bench/bench.md) for methodology and full results.

---

## Use Cases

| Scenario | Description |
|---|---|
| **AI Contact Center** | AI agents handle calls 24/7, escalate to humans |
| **Cloud Call Center** | Multi-tenant SaaS, remote agents, WebRTC + SIP |
| **Enterprise UC** | Internal comms, conferencing, CRM integration |
| **VoIP Wholesale** | Multi-carrier routing, flexible billing (Commerce) |
| **Compliance Recording** | PCI/healthcare recording, AI quality inspection |

---

## Architecture

![RustPBX Architecture](./docs/architecture.svg)

**App Service** (AI Agents, HTTP DialPlan, CRM) → **RustPBX Core** (B2BUA, IVR, Media, Queue, CDR) → **Access** (PSTN, WebRTC, SIP, Mobile)

---

## Build from Source

```bash
# Linux: apt-get install cmake pkg-config libasound2-dev libssl-dev libopus-dev
# macOS: brew install cmake openssl pkg-config

git clone --recurse-submodules https://github.com/restsend/rustpbx
cd rustpbx
cargo build --release
cargo run --bin rustpbx -- --conf config.toml.example
```

> Cross-compile via [cross](https://github.com/cross-rs/cross): `cargo install cross && cross build --release --target aarch64-unknown-linux-gnu`

### Submodules

Commerce addons are managed as git submodules under `src/addons/`:

| Submodule | Repository |
|-----------|-----------|
| `src/addons/cc` | https://cnb.cool/miuda.ai/cc |
| `src/addons/wholesale` | https://cnb.cool/miuda.ai/wholesale |
| `src/addons/endpoint_manager` | https://cnb.cool/miuda.ai/endpoint_manager |
| `src/addons/enterprise_auth` | https://cnb.cool/miuda.ai/enterprise_auth |
| `src/addons/ivr_editor` | https://cnb.cool/miuda.ai/ivr_editor |
| `src/addons/sbc` | https://cnb.cool/miuda.ai/sbc |
| `src/addons/telemetry` | https://cnb.cool/miuda.ai/telemetry |
| `src/addons/voicemail` | https://cnb.cool/miuda.ai/voicemail |

```bash
# Initialize submodules after clone
git submodule update --init --recursive

# Pull latest changes for all submodules
git submodule update --remote

# Pull latest for a specific submodule
git submodule update --remote src/addons/cc
```

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

| Guide | Description |
|---|---|---|
| [Configuration Guide](docs/configuration.md) | All config options |
| [Authentication](docs/config/03-auth-users.md) | User backends, JWT & HTTP token fast registration |
| [API Integration Guide](docs/api_integration_guide.md) | HTTP Router, Webhooks, Call Control, Recording |
| [RWI Protocol](docs/rwi.md) | WebSocket Interface |
| [RWI Events Reference](docs/rwi_events_reference.md) | Event types, fields, JSON examples (中文) |
| [RWI Events Reference (EN)](docs/rwi_events_reference_en.md) | Event types, fields, JSON examples (English) |

---

## Troubleshooting

**SIP 401 behind NAT/Docker** — set the realm explicitly:
```toml
[proxy]
realms = ["your-public-ip:5060"]
```

---

## License

Community: MIT · Commercial: [hi@miuda.ai](mailto:hi@miuda.ai)

**https://miuda.ai** — Maintenance & commercial support
