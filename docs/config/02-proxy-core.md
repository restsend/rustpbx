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

# Enable NAT media latching (helps with RTP behind NAT)
enable_latching = true

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
# Registrar expires time (in seconds)
registrar_expires = 3600

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
