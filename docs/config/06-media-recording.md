# Media, Recording & CDR

## Media Proxy
Controls how RTP traffic is handled.

```toml
[proxy]
# Modes: 
# - "auto": Bridge RTP only when necessary (e.g. WebRTC <-> UDP, or NAT detected)
# - "all":  Always bridge RTP (B2BUA style)
# - "nat":  Bridge only if private IP is detected
# - "none": Direct media (signaling only)
media_proxy = "auto"

# Codec Negotiation
codecs = ["opus", "pcmu", "pcma", "g729"]
```

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
callee_deny = ["911"]

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

## CDR Storage (`[callrecord]`)
Configure where post-call CDR JSON is stored or sent. This does not control recording media upload.

`max_concurrent` controls how many post-call CDR save/upload/hook tasks may run at once. The default is `64`; values below `1` are clamped to `1`.

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
