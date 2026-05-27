# SipFlow

SipFlow is the SIP flow capture subsystem built into RustPBX. It captures call SIP signalling messages into a compact on-disk store and can also capture bi-directional RTP media when no explicit recording policy is configured. The console and HTTP API use this data to query call flows and, when media capture is enabled, replay recordings by Call-ID.

> **Default configuration.** The example config (`config.toml.example`) enables
> `[sipflow]` with `type = "local"` and omits `[recording]`. This gives you SIP
> signalling capture **and** RTP audio capture out of the box. If you add a
> `[recording]` section, SipFlow will stop capturing RTP audio (signalling
> capture continues). See the "Recording Priority" section below.

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

## Recording Priority

> **WARNING: `[recording]` and `[sipflow]` are mutually exclusive for RTP
> capture.** If you configure both, SipFlow will capture SIP signalling but
> **NOT** RTP audio. To get recordings in the console audio player, either
> remove `[recording]` or ensure `[recording] enabled = true` writes WAV files.

SipFlow SIP message capture is independent from call recording. If `[sipflow]` is enabled, RustPBX continues to store SIP messages for the caller and callee Call-IDs.

RTP media capture follows the recording policy:

- If `[recording]` or `[proxy.recording]` is configured, that policy has priority. SipFlow RTP capture is disabled, even when `enabled = false`.
- If `[recording] enabled = true`, the live WAV recorder handles media recording and optional upload.
- If `[recording] enabled = false`, no RTP recording is produced.
- If no recording section is configured, SipFlow can capture RTP media and `[sipflow.upload]` can export the generated WAV after the call.

## Optional: Upload SipFlow Media

When no recording section is configured, the local backend can asynchronously export the captured SipFlow media as a WAV and upload it to S3-compatible storage or an HTTP endpoint.

For S3 uploads, RustPBX can pre-compute the final object URL and place it in the call record before the slower media upload finishes. The object may not be readable until the upload completes. After a successful upload, the same `recording_url` is updated with the final URL and duration.

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

## Enabling SipFlow

SipFlow capture is enabled by configuring the `[sipflow]` backend. It is independent from `[callrecord]`, which only controls CDR JSON storage.

```toml
# Default configuration (recommended):
# - SipFlow captures SIP signalling AND RTP audio
# - No [recording] section needed
[sipflow]
type = "local"
root = "./config/sipflow"
subdirs = "daily"
```

When SipFlow is active and **no `[recording]` section exists**, each CDR entry in the console shows both the **SIP Flow** tab (signalling ladder) and the **audio player** (recorded RTP). If a `[recording]` section is present, the audio player relies on the legacy recorder's WAV output instead.

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
