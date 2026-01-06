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
log_file = "/var/log/rustpbx.log"
```

## Database
Primary database connection. Currently supports SQLite (and MySQL in some builds).

```toml
# SQLite
database_url = "sqlite://rustpbx.sqlite3"

# MySQL
# database_url = "mysql://user:pass@localhost:3306/rustpbx"
```

## Network & NAT (RTP)
Crucial for audio handling. If behind NAT, `external_ip` MUST be set to your public IP.

```toml
# Public IP address (advertised in SDP)
external_ip = "203.0.113.10"

# RTP Port Range (UDP)
rtp_start_port = 12000
rtp_end_port = 42000

# ICE Servers (STUN/TURN) for WebRTC Clients
[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[ice_servers]]
urls = ["turn:turn.example.com:3478"]
username = "myuser"
credential = "mypassword"
```
