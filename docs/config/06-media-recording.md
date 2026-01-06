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
Control when calls are recorded.

```toml
[recording]
enabled = false
# Record these directions
directions = ["inbound", "outbound", "internal"]

# Auto-start recording on answer
auto_start = true

# Storage path for raw audio files
path = "./recordings"

# Fine-grained filters
caller_allow = ["1001", "1002"]
callee_deny = ["911"]
```

## CDR & Record Storage (`[callrecord]`)
Configure where metadata and recordings are permanently stored.

### Local Filesystem
```toml
[callrecord]
type = "local"
root = "./cdr_archive"
```

### S3 Compatible Object Storage
Uploads recordings to AWS S3, MinIO, DigitalOcean Spaces, etc.

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
```

### HTTP Webhook
Send CDR JSON to an endpoint.
```toml
[callrecord]
type = "http"
url = "http://my-crm/cdr-hook"
keep_media_copy = true
```
