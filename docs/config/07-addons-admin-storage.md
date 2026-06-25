# Addons, Console & Admin

## Admin Console
Built-in web management interface. 

The console allows administrators to monitor system health and manage core platform settings without editing TOML files manually. Key management features include:
- **Platform**: Logging, External IP, and RTP port ranges.
- **Proxy Settings**: Realms, User Authentication Backends, Locator Webhooks, and HTTP Routers with built-in testing tools.
- **Storage**: Media recording paths and S3 bucket integrations.
- **Security**: ACL rule management.

```toml
[console]
base_path = "/console"
# API prefix for REST endpoints (default: "/api")
api_prefix = "/api"
# Static files HTTP path prefix (default: "/static")
static_path = "/static"
session_secret = "change-me-random-string-must-be-long"

# Allow first user to create account?
allow_registration = false

# Cookie security
secure_cookie = false # Set true behind HTTPS proxy

# Default locale and available locales (i18n)
# locale_default = "en"
# [console.locales]
# en = { label = "English", flag = "🇬🇧" }
# zh = { label = "中文", flag = "🇨🇳" }

# API tokens for programmatic access to console REST APIs
# [[console.api_tokens]]
# token = "api-token-here"
# scopes = ["read", "write"]
# description = "Monitoring script token"

# Optional: override CDN URLs for frontend JS libraries.
# Configuring these to local paths or a nearby CDN mirror can significantly
# speed up page load / first-render time in restricted-network environments.
#
# Default values (loaded from public CDNs when not set):
#   alpine_js   = "https://cdnjs.cloudflare.com/ajax/libs/alpinejs/3.15.0/cdn.min.js"
#   tailwind_js = "https://cdnjs.cloudflare.com/ajax/libs/tailwindcss-browser/4.1.13/index.global.min.js"
#   chart_js    = "https://cdnjs.cloudflare.com/ajax/libs/Chart.js/4.5.0/chart.umd.min.js"
#   jssip_js    = "//jssip.net/download/releases/jssip-3.10.0.js"
#
# alpine_js   = "/static/js/alpine.min.js"
# tailwind_js = "/static/js/tailwind.min.js"
# chart_js    = "/static/js/chart.umd.min.js"
# jssip_js    = "/static/js/jssip.min.js"
```

## AMI (Asterisk Manager Interface)
TCP event stream for legacy integrations.

```toml
[ami]
# IP whitelist
allows = ["127.0.0.1", "10.0.1.10"]
```

## Generic Storage (`[storage]`)
Used by various addons (transcripts, wholesale exports, etc) to store blobs. Distinguishable from Call Recording storage.

```toml
# Local Storage
[storage]
type = "local"
path = "storage/blobs"

# S3 Storage
# [storage]
# type = "s3"
# vendor = "aws"
# bucket = "app-assets"
# region = "us-west-2"
# access_key = "..."
# secret_key = "..."
```

## Archive Addon
Auto-archives old data.

```toml
[archive]
enabled = true
archive_time = "03:00:00"
retention_days = 90
```

## Wholesale Addon
Example of addon-specific configuration.

```toml
# Enable the addon
[proxy]
addons = ["wholesale"]

# Configure the addon
[addons.wholesale]
billing_cycle = "monthly"
currency = "USD"
# bills_dir = "storage/wholesale/bills"  # Override CSV output directory
```

## Cluster Configuration

Multi-node clustering for high availability.

```toml
[cluster]
[[cluster.peers]]
addr = "10.0.0.1"
sip_port = 5060
ami_port = 8080

[[cluster.peers]]
addr = "10.0.0.2"
sip_port = 5060
ami_port = 8080
```

## Commercial Licenses

Map commercial addons to named license keys.

```toml
[licenses]
[licenses.addons]
wholesale         = "enterprise"
endpoint-manager  = "enterprise"
voicemail         = "basic"
ivr_editor        = "enterprise"

[licenses.keys]
enterprise = "LICENSE-KEY-XXXX-XXXX-XXXX"
basic      = "LICENSE-KEY-YYYY-YYYY-YYYY"
```

## RWI (Real-time WebSocket Interface)

Configuration for the RWI subsystem, which provides JSON-over-WebSocket call control. See [RWI Protocol](../rwi.md) for the full protocol reference.

```toml
[rwi]
enabled = true
max_connections = 2000
max_calls_per_connection = 200
orphan_hold_secs = 30
originate_rate_limit = 10

# API tokens for RWI access
[[rwi.tokens]]
token = "your-rwi-token"
scopes = ["call", "session", "media", "record", "conference", "queue"]

# Call contexts
[[rwi.contexts]]
name = "default"
no_answer_timeout_secs = 30
# no_answer_action = "transfer"
# no_answer_transfer_target = "sip:voicemail@local"

# Transfer behaviour
[rwi.transfer]
# refer_enabled = true
# attended_enabled = true
# three_pcc_fallback_enabled = true
# refer_timeout_secs = 30
# three_pcc_timeout_secs = 60
# max_concurrent_transfers = 1000
```

## RWI Webhook (`[rwi_webhook]`)

Top-level webhook for RWI real-time events (call started, call ended, media recorded, etc.). Uses the same structure as `[proxy.locator_webhook]`.

```toml
[rwi_webhook]
url = "https://events.example.com/rwi"
events = ["call.started", "call.ended", "media.recorded"]
headers = { "Authorization" = "Bearer token123" }
timeout_ms = 5000
```
