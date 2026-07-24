# SIP Proxy Core

The `[proxy]` section controls the core SIP signaling engine.

## Listeners & Transport
Configure which ports and protocols to enable.

```toml
[proxy]
addr = "0.0.0.0"

# Standard SIP ports
udp_port = 5060
# Multiple UDP ports (e.g., for multi-tenant or port range binding)
# udp_ports = [5060, 5062, 5064]
tcp_port = 5060

# Encrypted SIP (SIPS)
tls_port = 5061
# Uses proxy-level ssl_* configs (if not set, falls back to top-level ssl_*)
ssl_certificate = "./certs/fullchain.pem"
ssl_private_key = "./certs/privkey.pem"

# WebRTC (SIP over WebSocket)
ws_port = 8089
ws_handler = "/ws"

# RWI (RustPBX WebSocket Interface) — real-time call control WebSocket path
rwi_path = "/rwi/v1"

# AMI (Asterisk Manager Interface) HTTP path (default: "/ami/v1")
# ami_path = "/ami/v1"
```

## SIP Identity & Behavior

```toml
[proxy]
# SIP User-Agent header
useragent = "RustPBX/1.2.0"
# Appended to Call-ID (e.g., id@rustpbx.com)
callid_suffix = "rustpbx.com"
# Allowed domains (realms). Empty = allows all matching IP/Host.
realms = ["example.com", "sip.process-one.net"]
```

### The Role of Realms
Realms are used to partition the SIP namespace and provide context for authentication.
- **Security**: Challenges are issued against a specific realm. Users must provide credentials valid for that realm.
- **Domain Routing**: When multiple domains are hosted on the same IP, realms allow the proxy to distinguish which settings or user database to use.
- **Identity**: Often used as the domain part of a SIP URI (e.g., `user@realm`).

You can manage Realms in the **Web Console** under **Settings > Proxy Settings**. Changes require a service restart to take full effect.

## Concurrency & Protection

```toml
[proxy]
# Max simultaneous transactions handled
max_concurrency = 5000

# Reject matching User-Agents
ua_black_list = ["friendly-scanner", "pplsip"]
# Only allow specific User-Agents (if set, others are rejected)
ua_white_list = []

# If true, silently ignore requests to unknown users (anti-scanning)
ensure_user = true

# Frequency limiter (optional format: "100/60s" for 100 requests per 60 seconds)
frequency_limiter = "100/60s"

# SIP Session Timers (RFC 4028)
session_timer = true
# Keep refreshing session timer even if peer does not negotiate it
# session_timer_always = false
session_expires = 1800  # 30 minutes

# RTP timeout — if no audio packets are received on either direction for
# this many seconds, the call is automatically terminated (default: 30)
rtp_timeout = 30

# SIP Transaction Timers (RFC 3261) - optional overrides
t1_timer = 500      # T1 timer in milliseconds (default: 500)
t1x64_timer = 32000 # T1x64 timer in milliseconds (default: 32000)
```

### Worker Thread Isolation

RustPBX uses two dedicated tokio runtimes for SIP signalling and RTP media
forwarding. This prevents heavy media-plane load from starving SIP timer and
transaction tasks, which would otherwise cause 408 Request Timeout responses
under high concurrency (e.g. `mediaproxy = "all"` with thousands of calls).

```toml
[proxy]
# Number of worker threads for the SIP signalling runtime.
# Default: min(4, num_cpus) — typically 4 on multi-core machines.
sip_worker_threads = 4

# Number of worker threads for the RTP/media runtime.
# Default: num_cpus - sip_worker_threads.
media_worker_threads = 28
```

- **`sip_worker_threads`**: Threads dedicated to SIP message parsing, transaction
  management, and timer processing. 2–4 threads are sufficient for most
  deployments because signalling is lightweight.
- **`media_worker_threads`**: Threads dedicated to RTP/WebRTC media forwarding,
  SRTP encryption/decryption, transcoding, file playback, and recording I/O.
  Assign the majority of available cores here.

> **Note**: These settings are applied at startup only; changing them requires a
> restart. The two runtimes are completely independent — a busy media runtime
> will never block the SIP signaling runtime.

## Modules
Controls the loading of internal logic pipelines.

```toml
[proxy]
# Default stack
modules = ["acl", "auth", "registrar", "call"]

# Enable additional Addons
addons = ["wholesale", "monitor"]
```

## Media & Codecs

```toml
[proxy]
# Media proxy mode: auto, all, none, nat, bypass
#   auto:    Bridge RTP only when necessary (e.g., WebRTC ↔ UDP, or NAT detected)
#   all:     Always bridge RTP (B2BUA style)
#   nat:     Bridge only if private IP is detected
#   none:    Direct media (signaling only)
#   bypass:  Rewrite SDP but let RTP flow directly between endpoints
media_proxy = "auto"

# Preferred audio codec order for SDP negotiation
audio_codecs = ["opus", "pcmu", "pcma", "g729"]

# Video codec allowlist for re-INVITEs sent to RTP/PSTN/IMS trunks.
# WebRTC-only codecs (VP8, VP9, AV1, …) and all rtcp-fb feedback attributes
# are stripped automatically. Defaults to ["H264"] when not set.
# video_codecs = ["H264"]

# Codec selection strategy for WebRTC endpoints.
# "performance" (default): avoid transcoding, keep only caller-offered codecs.
# "quality": prefer Opus > G729 > G722 > G711 (may require transcoding).
codec_strategy = "performance"

# Enable NAT media latching (helps with RTP behind NAT, default: true)
enable_latching = true

# Maximum RTP packets to observe during latching probation before committing
# to a candidate source address. Higher values improve stability on flaky
# networks at the cost of slower latch convergence. (default: 6, only used
# when enable_latching = true)
latching_probation_max_packets = 6

# Enable NAT fix for SIP signaling
nat_fix = true
```

## File Loading & Paths

### File System Mode (default)

```toml
[proxy]
# Directory for auto-generated configs (managed by UI/API)
generated_dir = "./config"

# Load additional config files matching these patterns
routes_files = ["config/routes/*.toml"]
trunks_files = ["config/trunks/*.toml"]
acl_files = ["config/acl/*.toml"]
queues_files = ["config/queues/*.toml"]
ivr_files = ["config/ivr/*.toml"]

# Directory for queue-specific data files
queue_dir = "./queues"
```

### Database Mode

Set `generated_db = true` to store all generated configs in the application database instead of the filesystem. This is useful for:
- **High-availability deployments** where config must be shared across nodes
- **Kubernetes/container environments** with ephemeral filesystems
- **Multi-instance clusters** requiring centralized config management

```toml
[proxy]
generated_db = true
# generated_dir, routes_files, trunks_files, etc. are ignored
# in DB mode — all generated configs use the config_entries table.
```

In this mode, the following config types are written to and loaded from the `config_entries` database table:

| Config Type | Category | Entry Name |
|-------------|----------|-----------|
| Trunks | `trunks` | `trunks.generated.toml` |
| Routes | `routes` | `routes.generated.toml` |
| Queues | `queue` | `queues.generated.toml` |
| ACL | `acl` | `acl.generated.toml` |
| IVR projects | `ivr` | `{name}.generated.toml` |
| CC ACD | `cc_acd` | `acd.toml` |
| CC Skill Groups | `cc_skill_groups` | `skill_groups.generated.toml` |
| CC Agents | `cc_agents` | `agents.generated.toml` |
| CC Transfer | `cc_transfer` | `transfer.toml` |
| SBC JSON-RPC | `sbc` | `sbc_jsonrpc.toml` |

> **Note**: There is no migration path between modes. Switching from filesystem to DB mode (or vice versa) requires re-exporting all configs through the admin console.

### Inline ACL Rules

### Inline ACL Rules

ACL rules can also be defined inline in `rustpbx.toml` alongside `acl_files`:

```toml
[proxy]
# Inline rules are merged with rules loaded from acl_files
acl_rules = [
    "allow all",
    "deny all",
]
```

## Call Handling

```toml
[proxy]
# Registrar default expires time in seconds (default: 30).
# This is the fallback value when the REGISTER request does not include
# an Expires header or Contact expires parameter.
# Both settings can be changed via the Web Console under Settings > Proxy.
registrar_expires = 30

# Maximum allowed expires value in seconds (default: 50).
# Client-requested expires exceeding this limit will be capped.
# Set to a higher value if clients need longer registration lifetimes.
max_registrar_expires = 50

# Passthrough failure status codes to caller
# When true, caller receives the same SIP error code (e.g., 486, 603) from callee
# When false, a generic error code is sent
passthrough_failure = true

# Use SIP REFER for blind transfers (default: false, uses re-INVITE)
# blind_transfer_use_refer = false

# Maximum items for SIP flow storage (per dialog)
sip_flow_max_items = 1000
```

## In-Dialog Authentication Cache

When enabled, successfully authenticated dialogs are cached along with their source address. Subsequent in-dialog requests (e.g., re-INVITE, BYE) from the same source address within the TTL window skip re-authentication, reducing latency and load.

```toml
[proxy]
# Enabled by default. Set to false to disable.
dialog_auth_cache = { enabled = true, cache_size = 10000, ttl_seconds = 3600 }
```

- **`enabled`**: Whether to skip authentication for cached in-dialog requests. Default: `true`.
- **`cache_size`**: Maximum number of dialogs to cache (LRU eviction). Default: `10000`.
- **`ttl_seconds`**: Time-to-live for cache entries. Default: `3600` (1 hour).

To disable explicitly:

```toml
[proxy]
dialog_auth_cache = { enabled = false }
```

## Channel Capacity

Controls the size of internal async channels used for session and media command/event passing. Increasing these values may help under high concurrency at the cost of memory, while decreasing them can reduce backpressure latency.

```toml
[proxy]
# Session command channel capacity (default: 256)
session_cmd_channel_capacity = 256
# Session state change channel capacity (default: 256)
session_state_channel_capacity = 256
# Media engine command channel capacity (default: 512)
media_cmd_channel_capacity = 512
# Media engine event channel capacity (default: 1024)
media_event_channel_capacity = 1024
```

- **`session_cmd_channel_capacity`**: Max pending commands per SIP session (e.g., hangup, transfer, play). Default: `256`.
- **`session_state_channel_capacity`**: Max pending state-change notifications per session. Default: `256`.
- **`media_cmd_channel_capacity`**: Max pending commands per media engine instance (e.g., play, stop, record). Default: `512`.
- **`media_event_channel_capacity`**: Max pending media events per engine instance (e.g., DTMF, playback complete). Default: `1024`.

## DoS Protection

Rate-limiting and anti-scanning controls for SIP traffic.

```toml
[proxy]
# Enable DoS protection
dos_enabled = false

# Max calls per second per source IP (default: 100)
dos_max_cps_per_ip = 100

# Max concurrent dialogs per source IP (default: 500)
dos_max_concurrent_per_ip = 500

# Number of failed probe attempts before blocking (default: 50)
dos_scan_probe_threshold = 50

# Block duration in seconds for detected scanners (default: 600)
dos_scan_block_duration_secs = 600
```

## SBC / Trusted Proxy Support

When RustPBX sits behind a SIP proxy or SBC, the socket-level source
address is always the proxy's IP, which breaks ACL matching, trunk
identification, and DoS tracking. Use `trusted_proxies` to extract
the real client IP from the Via header chain.

```toml
[proxy]
# List of trusted proxy/SBC IPs or CIDR networks.
# When a request arrives from one of these addresses, the real client
# IP is extracted from the Via chain instead of using the socket IP.
# Leave empty (default) if no proxy is in front of PBX.
trusted_proxies = ["10.0.0.1", "10.0.0.0/24"]
```

### How It Works

1. If the socket-level source IP does **not** match any entry in
   `trusted_proxies`, it is used directly (no proxy scenario).

2. If it matches, PBX walks the Via header chain, skipping the first
   entry (the directly-connected proxy) and any subsequent entries
   whose sent-by IP also matches `trusted_proxies` (multi-hop SBC).

3. The first Via entry **not** in the trusted list is treated as the
   client:
   - If it has a `received=` parameter (RFC 3581, set when the client
     was behind NAT), that IP is used.
   - Otherwise, the Via sent-by IP is used.

4. If all Via entries belong to trusted proxies, falls back to the
   socket IP.

### Example: Single SBC

```
Client (1.2.3.4) → SBC (10.0.0.1) → PBX

Via: SIP/2.0/UDP 10.0.0.1:5060;branch=sbc
Via: SIP/2.0/UDP 1.2.3.4:5060;received=1.2.3.4;branch=client
```

- Socket IP = 10.0.0.1 → matches `trusted_proxies`
- Skip entry[0] (10.0.0.1, matches trusted)
- entry[1] = 1.2.3.4 → not trusted → client!
- `received=1.2.3.4` present → use received IP

### Example: Client Without NAT

```
Client (1.2.3.4, no NAT) → SBC (10.0.0.1) → PBX

Via: SIP/2.0/UDP 10.0.0.1:5060;branch=sbc
Via: SIP/2.0/UDP 1.2.3.4:5060;branch=client    (no received=)
```

- Same process, but no `received=` → use sent-by IP 1.2.3.4

### Example: Multi-Hop SBC

```
Client (1.2.3.4) → SBC1 (10.0.0.2) → SBC2 (10.0.0.1) → PBX

Via: SIP/2.0/UDP 10.0.0.1:5060;branch=sbc2
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=sbc1
Via: SIP/2.0/UDP 1.2.3.4:5060;received=1.2.3.4;branch=client
```

- Socket IP = 10.0.0.1 → matches trusted
- Skip entry[0] (10.0.0.1, matches trusted)
- entry[1] = 10.0.0.2 → matches trusted → skip
- entry[2] = 1.2.3.4 → not trusted → client!

### Security

Only enable `trusted_proxies` when the PBX listener is firewalled so
that all traffic must arrive through the trusted SBC/proxy. If an
attacker can send requests directly to the PBX, they should not match
any `trusted_proxies` entry (their socket IP will be used directly).

## URI Validation

```toml
[proxy]
# Maximum URI length accepted (default: 256)
uri_max_length = 256

# Reject malformed URIs (default: false)
uri_reject_malformed = false
```

## Emergency Numbers

Configure emergency call handling for local compliance.

```toml
[proxy]
[proxy.emergency]
enabled = true
numbers = ["110", "119", "120", "122", "911", "999"]
emergency_trunk = "pstn-trunk"
```

## Identity & Privacy

These settings control how the PBX identifies itself in SIP signaling and SDP media attributes.

```toml
[proxy]
# Contact header username when no dialplan caller_contact is set.
# If not specified, a random 16-char hex string is generated at startup.
contact_username = "my-pbx-01"

# CNAME value used in SDP a=ssrc:<n> cname:<value> attributes.
# If not specified, a random 16-char hex string is generated at startup.
# This replaces the default "rustrtc-cname-<ssrc>" in generated SDP.
rtc_cname = "my-pbx-01"
```

When neither is specified, both default to the same randomly generated hex string, making the PBX instance identifiable without revealing implementation details.

---

## Session Hooks

Session hooks allow addons to observe and influence the call lifecycle. Hooks are registered
programmatically via [`SipServerBuilder::with_session_hook`].

### Built-in Hooks

| Hook | Module | Trigger | Purpose |
|------|--------|---------|---------|
| `CcCallSessionHook` | `addons::cc` | connected / held / ended / agent_disconnected | CC lifecycle events + CDR push |
| `IvrExecHook` | `proxy::proxy_call::ivr_exec_hook` | `on_app_exited` | IVR exec auto-unhold + result delivery |

### IvrExecHook

The `IvrExecHook` handles the `ivr.exec` post-exit flow: when an application started via
`ivr.exec` exits, the hook reads the execution state from session extensions, writes
the result (or sends a webhook), and instructs the session to unhold the callee and
send a result SIP INFO back.

Registration (already done automatically when CC addon is enabled):

```rust
use std::sync::Arc;
use crate::proxy::proxy_call::ivr_exec_hook::IvrExecHook;

SipServerBuilder::new(config)
    .with_session_hook(Arc::new(IvrExecHook))
    // ...
```

For the complete `ivr.exec` protocol reference, see [`docs/ivr_exec.md`](../ivr_exec.md).
