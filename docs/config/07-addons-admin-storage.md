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
session_secret = "change-me-random-string-must-be-long"

# Allow first user to create account?
allow_registration = false

# Cookie security
secure_cookie = false # Set true behind HTTPS proxy

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

# Separate config file often used
# config/wholesale.toml
[cluster]
peers = ["http://node2:8080"]
```
