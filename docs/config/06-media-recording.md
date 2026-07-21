# Media, Recording & CDR

## Media Proxy

Controls how RTP traffic is handled. Configurable at two levels:

### Server-level (`[proxy]`)

Sets the default for all calls. Trunk-level config overrides this when present.

```toml
[proxy]
# Modes: 
# - "auto": Bridge RTP only when necessary (e.g. WebRTC <-> UDP, or NAT detected)
# - "all":  Always bridge RTP (B2BUA style)
# - "nat":  Bridge only if private IP is detected
# - "none": Direct media (signaling only)
# - "bypass": SDP rewrite only, RTP direct (experimental)
media_proxy = "auto"

# Codec Negotiation
codecs = ["opus", "pcmu", "pcma", "g729"]

# RTP external/bind IP
# external_ip = "203.0.113.1"   # IP advertised in SDP c=/o= and ICE candidates
# auto_external_ip = "http://ifconfig.me"  # Auto-detect external IP
# bind_ip = "0.0.0.0"           # Local RTP socket bind address

# RTP port range
# rtp_start_port = 10000
# rtp_end_port   = 20000

# Latching (learn callee's actual RTP address from first received packet)
# enable_latching = true
# latching_probation_max_packets = 6
```

### Per-trunk override (`[proxy.trunks.<name>]`)

Overrides the server-level `media_proxy` for calls routed through this trunk.
Useful when some trunks need different media handling (e.g. overlay-network call
termination, or forcing anchor for NAT-prone endpoints).

```toml
[proxy.trunks.overlay_trunk]
dest = "sip:overlay.example:5060"

# Trunk-level media mode (overrides server's media_proxy):
# - "auto":           bridge only for app/queue flows
# - "none":           no media proxy (SDP passthrough, RTP direct)
# - "bypass":         SDP rewrite only, RTP direct
# - "force_transcode": always bridge through PBX (equivalent to server "all")
media_mode = "force_transcode"

# Per-trunk SDP IP override (overrides server's external_ip):
# Use when this trunk terminates on an overlay network (Tailscale/WireGuard)
# that needs a different advertised IP than the public NAT address.
external_ip = "100.64.10.1"
bind_ip = "100.64.10.2"

# Codecs (overrides server-level codecs)
codec = ["opus", "pcmu"]
```

### Latching

When `enable_latching = true` (default), the PBX learns a callee's actual RTP
address from the first received packet, even if the callee's SDP advertises a
private LAN IP. This makes `media_proxy = "auto"` or `"force_transcode"` work
correctly with NAT'd endpoints without needing explicit SDP rewriting.

**Server-level defaults:**
```toml
[proxy]
enable_latching = true
latching_probation_max_packets = 6
```

**Per-trunk override:**
```toml
[proxy.trunks.example]
enable_latching = false
```

### Choosing the right combination (Bug 1 + 2 scenarios)

| Scenario | Server `media_proxy` | Trunk `media_mode` | Trunk `external_ip` | Result |
|----------|---------------------|--------------------|--------------------|--------|
| Normal SIP trunk (public) | `auto` (default) | — | — | Bypass for direct calls, anchor for app/queue |
| Overlay-network trunk | `auto` | `force_transcode` | `100.64.x.x` | Anchored through PBX, overlay IP in SDP |
| Outbound trunk (NAT'd callee) | `auto` | `force_transcode` | — | Anchored + latching handles NAT |
| Direct extension calls | `auto` | — | — | Bypass, SIP handles NAT naturally |
## Recording Policy

> **[recording] and [sipflow] are mutually exclusive for RTP capture.**
> The default configuration uses `[sipflow]` for both SIP signalling and RTP
> audio capture. Only configure `[recording]` when you specifically need the
> legacy live WAV recorder. See [08-sipflow.md](08-sipflow.md) for details.

Control when calls are recorded. Can be set at top-level `[recording]` or per-proxy `[proxy.recording]` (proxy-level overrides top-level).

`[recording]` controls the live WAV recorder. When enabled, the recorder always writes a local WAV first. Set `type = "http"` or `type = "s3"` only to upload that local WAV after the call completes.

Recording configuration has priority over SipFlow RTP recording. If a top-level `[recording]` or per-proxy `[proxy.recording]` section is configured, it owns the recording decision:

- `enabled = true`: use the live WAV recorder and optional `[recording]` upload.
- `enabled = false`: do not record RTP media.
- SipFlow SIP message capture still works when `[sipflow]` is enabled.
- SipFlow RTP capture and `[sipflow.upload]` recording export are disabled for that call.

Only omit the recording section entirely when you want SipFlow to capture RTP audio and/or `[sipflow.upload]` to act as the recording source.

```toml
# Top-level recording config (applies to all proxies unless overridden)
[recording]
enabled = false

# Recording upload mode: "local" (default), "http", or "s3".
type = "local"

# Record these directions
directions = ["inbound", "outbound", "internal"]

# Auto-start recording on answer
auto_start = true

# Storage path for raw audio files
path = "./recordings"

# Optional local filename template. Supported tokens:
# {session_id}, {caller}, {callee}, {direction}, {timestamp}
filename_pattern = "{session_id}"

# Fine-grained filters
caller_allow = ["1001", "1002"]
caller_deny = ["anonymous"]
callee_allow = []
callee_deny = ["911"]

# Recording quality
samplerate = 8000       # Audio sample rate (Hz)
ptime = 20              # Packetization time (ms)

# Hybrid mode: force legacy WAV recorder even when [sipflow] is active
# When true, SipFlow captures signalling only; [recording] handles media
# force_file = true

# Or configure per-proxy
[proxy.recording]
enabled = true
directions = ["inbound"]
auto_start = true
```

### HTTP Recording Upload
```toml
[recording]
enabled = true
auto_start = true
type = "http"
path = "./recordings"
url = "https://archive.example.com/recording"
# headers = { "Authorization" = "Bearer token" }
```

### S3 Recording Upload
```toml
[recording]
enabled = true
auto_start = true
type = "s3"
path = "./recordings"
vendor = "minio" # aws, gcp, azure, aliyun, tencent, minio, digitalocean
bucket = "recordings"
region = "us-east-1"
access_key = "MINIO_ACCESS_KEY"
secret_key = "MINIO_SECRET_KEY"
endpoint = "http://minio:9000"
root = "recordings"
```

When `[recording] type = "http"` or `type = "s3"` is used, the CDR may be written before the media upload finishes. The database `recording_url` is updated after the upload succeeds. The local CDR JSON keeps the local recorder path in `recordingUrl` and the recorder metadata in `recorder[]`.

## CDR (Call Detail Records)

### Database CDR (always on)

Every call is automatically persisted to the `rustpbx_call_records` table in your configured database. This is the primary CDR mechanism and requires no extra configuration — the Web Console "Call Records" page reads from this table.

As long as `database_url` is set (which is always required), call records will be written.

### Optional CDR sinks (`[callrecord]`)

The `[callrecord]` section is **optional**. It adds a secondary raw-CDR sink on top of the always-on database persistence. Omit this section entirely if you only need database CDRs (the common case).

`max_concurrent` controls how many post-call CDR save/upload/hook tasks may run at once. The default is `64`; values below `1` are clamped to `1`.

### Database
Writes CDR JSON to a separate database table (default: `call_records`).

```toml
[callrecord]
type = "database"
# database_url = "sqlite://cdr.sqlite3"     # Optional: separate database
# table_name = "call_records"               # Optional: custom table name
max_concurrent = 64
```

### Optional storage types

All examples below are **optional** add-ons. The database CDR (above) is always active regardless.

### Local Filesystem
```toml
[callrecord]
type = "local"
root = "./cdr_archive"
max_concurrent = 64
```

### S3 Compatible Object Storage
Uploads CDR JSON to AWS S3, MinIO, DigitalOcean Spaces, etc.

```toml
[callrecord]
type = "s3"
vendor = "minio" # aws, gcp, azure, digitalocean, etc.
bucket = "my-recordings"
region = "us-east-1"
# S3 Credentials
access_key = "MINIO_ACCESS_KEY"
secret_key = "MINIO_SECRET_KEY"
endpoint = "http://minio:9000" # needed for non-AWS
root = "/daily-records"
max_concurrent = 64

# Deprecated and ignored. Recording media upload is configured by [recording].
with_media = true
keep_media_copy = false
```

### HTTP Webhook
Send CDR JSON to an endpoint.
```toml
[callrecord]
type = "http"
url = "http://my-crm/cdr-hook"
max_concurrent = 64

# Deprecated and ignored. Recording media upload is configured by [recording].
with_media = true
keep_media_copy = false
```

HTTP CDR delivery uses `multipart/form-data` with field `calllog.json`. Recording media is delivered separately by `[recording] type = "http"`.
