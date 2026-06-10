# SIP Proxy Core

The `[proxy]` section controls the core SIP signaling engine.

## Listeners & Transport
Configure which ports and protocols to enable.

```toml
[proxy]
addr = "0.0.0.0"

# Standard SIP ports
udp_port = 5060
tcp_port = 5060

# Encrypted SIP (SIPS)
tls_port = 5061
# Uses proxy-level ssl_* configs (if not set, falls back to top-level ssl_*)
ssl_certificate = "./certs/fullchain.pem"
ssl_private_key = "./certs/privkey.pem"

# WebRTC (SIP over WebSocket)
ws_port = 8089
ws_handler = "/ws"
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
session_expires = 1800  # 30 minutes

# RTP timeout — if no audio packets are received on either direction for
# this many seconds, the call is automatically terminated (default: 30)
rtp_timeout = 30

# SIP Transaction Timers (RFC 3261) - optional overrides
t1_timer = 500      # T1 timer in milliseconds (default: 500)
t1x64_timer = 32000 # T1x64 timer in milliseconds (default: 32000)
```

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
# Media proxy mode: auto, all, none, nat
media_proxy = "auto"

# Preferred codec order for SDP negotiation
codecs = ["opus", "pcmu", "pcma", "g729"]

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

```toml
[proxy]
# Directory for auto-generated configs (managed by UI/API)
generated_dir = "./config"

# Load additional config files matching these patterns
routes_files = ["config/routes/*.toml"]
trunks_files = ["config/trunks/*.toml"]
acl_files = ["config/acl/*.toml"]

# Directory for queue-specific data files
queue_dir = "./queues"
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
