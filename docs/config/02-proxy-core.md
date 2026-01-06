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
# Certificates shared with HTTPS config or specified here overrides? 
# Usually uses top-level ssl_* unless specified in proxy? 
# (Check code: code uses top-level ssl certs if proxy ones aren't specific, usually shared)
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

# SIP Session Timers (RFC 4028)
session_timer = true
session_expires = 1800  # 30 minutes
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
