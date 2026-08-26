# Platform & Networking

## HTTP & HTTPS
Configures the internal web server for API, Management Console, and Webhook handling.

```toml
# Main listener
http_addr = "0.0.0.0:8080"
# Enable GZIP compression for HTTP responses
http_gzip = true

# Optional: HTTPS listener
https_addr = "0.0.0.0:8443"
ssl_certificate = "./certs/fullchain.pem"
ssl_private_key = "./certs/privkey.pem"

# Security: Skip access logs for health checks or metrics
http_access_skip_paths = ["/health", "/metrics"]
```

## Logging
Global logging configuration.

```toml
# Levels: debug, info, warn, error
log_level = "info"

# If unset, logs to stderr
log_file = "/var/log/rustpbx/app.log"

# Log rotation policy (only effective when log_file is set).
# Allowed values:
#   "never"  – single file, no rotation (default)
#   "daily"  – rotate once per day; filename suffix: YYYY-MM-DD
#   "hourly" – rotate once per hour; filename suffix: YYYY-MM-DD-HH
log_rotation = "daily"
```

> **Note on `log_file` + rotation**: `log_file` is treated as a *prefix*.
> For example, with `log_file = "/var/log/rustpbx/app.log"` and `log_rotation = "daily"`,
> the actual file written will be `/var/log/rustpbx/app.log.2026-04-10`.
> The directory must exist and be writable before the process starts.
> Old rotated files are **not** deleted automatically — use `logrotate` or similar tools
> for retention policies.

## Media Cache
Local cache directory for media files (e.g., ringback tones, IVR prompts). Managed via the Console UI; not parsed into the `Config` struct.

```toml
media_cache_path = "./config/mediacache"
```

## Database
Primary database connection. Supports SQLite, PostgreSQL, and MySQL.

```toml
# SQLite (default)
database_url = "sqlite://rustpbx.sqlite3"

# PostgreSQL
# database_url = "postgres://user:pass@localhost:5432/rustpbx"

# MySQL
# database_url = "mysql://root@localhost:3306/rustpbx"
```

### Database Connection Pool

Controls the connection pool for PostgreSQL/MySQL databases. SQLite connections
are single-connection and ignore these settings. When this section is omitted,
`max_connections` defaults to 64.

```toml
[database_pool]
max_connections = 64       # Maximum pool size (default: 64)
min_connections = 0        # Minimum idle connections
acquire_timeout_secs = 30  # Timeout in seconds to acquire a connection from pool
idle_timeout_secs = 600    # Max idle time (seconds) before closing; None = no limit
max_lifetime_secs = 1800   # Max connection lifetime (seconds); None = no limit
```

## Demo Mode

```toml
# When true, a demo superuser account is auto-created on startup
# and some addons run in evaluation mode (e.g. ACME bypasses
# certificate verification).
demo_mode = false
```

## Network & NAT

RustPBX separates **media (RTP/SDP)** and **signaling (SIP Contact)** external addresses. This avoids LAN hairpin failures where BYE is sent to a public IP that does not reach the PBX process (#244).

### RTP / SDP (media)

```toml
# Public IP advertised in SDP c=/o= and ICE candidates
external_ip = "203.0.113.10"

# Auto-detect RTP external IP (mutually exclusive with external_ip)
# auto_external_ip = "http://ifconfig.me"

# Defaults: 12000–42000 (~30k ports; plan roughly 2 RTP ports per concurrent call)
rtp_start_port = 12000
rtp_end_port = 42000
# webrtc_port_start = 30000
# webrtc_port_end = 40000
```

> **Profile `bind_ip` fallback**: when a profile omits `bind_ip`, the runtime uses the global RTP bind address (currently `[proxy].addr`) before falling back to `[proxy].addr` explicitly.

### SIP Contact (signaling)

```toml
# Optional dedicated Contact host for WAN peers (defaults to external_ip when unset)
# sip_external_ip = "203.0.113.10"
# auto_sip_external_ip = "http://ifconfig.me"

# Always use [proxy].addr in Contact (pure LAN / no NAT hairpin)
# sip_contact_always_bind = true

# CIDR list for "local" peers (empty = RFC1918 + loopback defaults)
# local_networks = ["192.168.0.0/16", "10.0.0.0/8"]

# When true (default), LAN destinations get bind address in Contact
contact_lan_use_bind = true
```

Configure both sections from **Console → Settings → Platform** (RTP external IP, SIP Contact IP, local networks).

### Network profiles (multi-path egress)

For deployments with multiple egress paths (public WAN, Tailscale/WireGuard overlay, etc.), define named profiles and bind trunks to them. When `[[network_profile]]` is empty, a synthetic `default` profile is derived from the top-level fields above.

```toml
default_network_profile = "wan"

[[network_profile]]
id = "wan"
label = "Public WAN"
external_ip = "203.0.113.10"
sip_external_ip = "203.0.113.10"
local_networks = ["192.168.0.0/16"]
rtp_start_port = 12000
rtp_end_port = 42000

[[network_profile]]
id = "overlay"
label = "Tailscale"
external_ip = "100.64.0.5"
bind_ip = "100.64.0.5"
contact_lan_use_bind = true
```

When a profile omits `bind_ip`, the runtime uses the global RTP bind address (same as `[proxy].addr` today) rather than forcing a different value.

Trunks reference a profile via TOML `profile = "overlay"` or Console trunk **Media Option → Network profile**. Per-trunk `external_ip` / `bind_ip` still override the profile when set.

Manage profiles from **Console → Settings → Network profiles**.

### ICE servers

```toml
[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[ice_servers]]
urls = ["turn:turn.example.com:3478"]
username = "myuser"
credential = "mypassword"

# ice_servers_path = "/iceservers"
```
