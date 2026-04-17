# SipFlow

SipFlow is the unified SIP + RTP recording subsystem built into RustPBX. It captures every call's SIP signalling messages and bi-directional RTP media into a compact on-disk store, then exposes an HTTP API for querying call flows and replaying recordings by Call-ID.

See [docs/sipflow.md](../sipflow.md) for the full deployment guide (architecture diagrams, systemd unit, storage sizing, and API reference).

---

## Backends

### Local (embedded)

SipFlow runs as a background task inside the RustPBX process — no separate service needed.

```toml
[sipflow]
type = "local"

# Directory where SQLite index and .raw payload files are written.
root = "./config/sipflow"

# Subdirectory layout: "none" | "daily" | "hourly"
# - "none"   — all files directly in root (low traffic / testing)
# - "daily"  — YYYYMMDD/ subdirectories (recommended default)
# - "hourly" — YYYYMMDD/HH/ subdirectories (very high traffic)
subdirs = "daily"

# Write a batch to disk after this many packets (default: 1000).
flush_count = 1000

# Force a flush if no batch has been written within this many seconds (default: 5).
flush_interval_secs = 5

# Size of the Call-ID LRU cache used to map Call-IDs to SQLite row IDs (default: 8192).
id_cache_size = 8192
```

### Remote (standalone `sipflow` binary)

Use this when multiple RustPBX nodes share a single SipFlow storage server.

```toml
[sipflow]
type = "remote"

# UDP address of the standalone sipflow server.
udp_addr = "192.168.1.100:3000"

# HTTP address of the standalone sipflow server (used for query API).
http_addr = "http://192.168.1.100:3001"

# Query timeout in seconds (default: 10).
timeout_secs = 10
```

Start the standalone server:

```sh
./sipflow \
  --addr 0.0.0.0 \
  --port 3000 \
  --http-port 3001 \
  --root /data/sipflow \
  --flush-count 1000 \
  --flush-interval 5 \
  --buffer-size 100000
```

---

## Optional: Upload to Object Storage

The local backend can asynchronously upload completed files to S3-compatible storage or an HTTP endpoint.

### S3

```toml
[sipflow.upload]
type = "s3"
vendor = "aws"          # "aws" | "minio" | "aliyun" | etc.
bucket = "my-sipflow"
region = "us-east-1"
access_key = "AKID..."
secret_key = "..."
endpoint = "https://s3.amazonaws.com"
root = "sipflow/"       # key prefix inside the bucket
```

### HTTP

```toml
[sipflow.upload]
type = "http"
url = "https://archive.example.com/upload"
# headers = { "Authorization" = "Bearer token" }
```

---

## Enabling SipFlow per Call Record

SipFlow recording is enabled on a per-call-record basis via the `enable_sipflow` flag in `[callrecord]`:

```toml
[callrecord]
type = "local"
root = "./config/cdr"
enable_sipflow = true
```

When set, each CDR entry in the console will display a **SIP Flow** tab with the signalling ladder and an audio player.

---

## Storage Layout

With `subdirs = "daily"`, files are organised as:

```
./config/sipflow/
└── 20260417/
    ├── 20260417.sqlite   # index: Call-ID → (timestamp, byte offset, size)
    └── 20260417.raw      # payload: SIP messages + RTP frames (zstd compressed)
```

Older directories can be deleted freely without affecting running calls:

```sh
# Remove data older than 7 days
find /data/sipflow -maxdepth 1 -type d -name '[0-9]*' \
  -mtime +7 -exec rm -rf {} +
```

---

→ Next: back to [00-overview.md](00-overview.md)
