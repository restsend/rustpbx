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

## Network & NAT (RTP)
Crucial for audio handling. If behind NAT, `external_ip` MUST be set to your public IP.

```toml
# Public IP address (advertised in SDP)
external_ip = "203.0.113.10"

# Auto-detect external IP via HTTP service (mutually exclusive with external_ip)
# At startup, a GET request is sent to this URL and the response body is parsed as IP
# Leave empty to use the default: http://ifconfig.me
# auto_external_ip = "http://ifconfig.me"

# RTP Port Range (UDP) — used for standard SIP media
rtp_start_port = 12000
rtp_end_port = 42000

# WebRTC Port Range (UDP) — separate range for WebRTC media
# webrtc_port_start = 30000
# webrtc_port_end = 40000

# ICE Servers (STUN/TURN) for WebRTC Clients
[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[ice_servers]]
urls = ["turn:turn.example.com:3478"]
username = "myuser"
credential = "mypassword"

# Custom HTTP path for the ICE servers config endpoint (default: "/iceservers")
# ice_servers_path = "/iceservers"
```
