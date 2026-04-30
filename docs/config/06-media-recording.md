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
Control when calls are recorded. Can be set at top-level `[recording]` or per-proxy `[proxy.recording]` (proxy-level overrides top-level).

`[recording]` controls the live WAV recorder. When enabled, the recorder always writes a local WAV first. Set `type = "http"` or `type = "s3"` only to upload that local WAV after the call completes.

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

If `[sipflow]` is active, RustPBX disables the live WAV recorder for the call so SIP+RTP capture is not duplicated. Use SipFlow export/upload for media in that mode.

## CDR Storage (`[callrecord]`)
Configure where post-call CDR JSON is stored or sent. This does not control recording media upload.

### Local Filesystem
```toml
[callrecord]
type = "local"
root = "./cdr_archive"
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

# Deprecated and ignored. Recording media upload is configured by [recording].
with_media = true
keep_media_copy = false
```

HTTP CDR delivery uses `multipart/form-data` with field `calllog.json`. Recording media is delivered separately by `[recording] type = "http"`.
