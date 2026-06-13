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

#### Storage Engine

The local backend supports two storage engines, selectable via the `engine` field:

| Engine | Description | Write Model | Best For |
|---|---|---|---|
| **`flowdb`** (default) | LSM-tree embedded key-value store (`flowdb` crate) | Synchronous `write_batch_sync()` per packet | High-throughput capture, compact storage |
| **`sqlite`** (legacy) | SQLite index + zstd-compressed raw payload file | Async channel → background batch INSERT | Backward compatibility, existing data |

```toml
[sipflow]
type = "local"
root = "./config/sipflow"
engine = "flowdb"   # "flowdb" (default) or "sqlite"
```

##### FlowDB engine options

When `engine = "flowdb"`, the following tuning parameters are available:

```toml
[sipflow]
type = "local"
engine = "flowdb"

# Auto-expire records after N seconds (optional, disabled by default).
# When set, FlowDB automatically garbage-collects expired SST entries.
# ttl_secs = 86400

# Memtable size threshold before flush to SSTable (default: 64 MB).
# Smaller values reduce memory usage at the cost of more SST files.
# memtable_size_mb = 64

# LRU block cache capacity for reads (default: 128 MB).
# Larger cache improves query performance on large datasets.
# block_cache_capacity_mb = 128
```

##### SQLite engine options

When `engine = "sqlite"`, the legacy options apply:

```toml
[sipflow]
type = "local"
engine = "sqlite"

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

> **Note:** `subdirs`, `flush_count`, `flush_interval_secs`, and `id_cache_size`
> are ignored when `engine = "flowdb"`.

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

## Performance Comparison: FlowDB vs SQLite

A built-in benchmark (`examples/sipflow_bench.rs`) compares the two engines using a realistic mixed workload (SIP INVITE messages ~500 bytes + RTP PCMA packets ~172 bytes per leg).

```sh
cargo run --release --example sipflow_bench -- --calls 200 --rtp-per-call 2000 --sip-per-call 30
```

### Results (FlowDB 0.1.6)

#### Scale 1 — 51 000 records (50 calls × 20 SIP + 1000 RTP)

| Metric | FlowDB | SQLite | Ratio |
|---|---:|---:|---:|
| Write throughput | **153 265 rec/s** | 13 078 rec/s | **11.7× faster** |
| Write time | 0.33 s | 3.90 s | — |
| Flow query latency | **0.2 ms** | 16.2 ms | **88× faster** |
| Disk usage | 14.7 MB | 5.8 MB | comparable |

#### Scale 2 — 406 000 records (200 calls × 30 SIP + 2000 RTP)

| Metric | FlowDB | SQLite | Ratio |
|---|---:|---:|---:|
| Write throughput | **189 469 rec/s** | 11 224 rec/s | **16.9× faster** |
| Write time | 2.14 s | 36.17 s | — |
| Flow query latency | **1.6 ms** | 24.1 ms | **14.7× faster** |
| Disk usage | 51.2 MB | 46.8 MB | comparable |

### Key Takeaways

- **Write throughput**: FlowDB achieves **12–17× higher write throughput** than the SQLite backend. FlowDB writes are sequential append-only WAL + memtable operations, while SQLite must parse each packet, compress payloads with zstd, and execute SQL INSERT transactions.
- **Query performance**: FlowDB is **15–88× faster** for flow queries thanks to prefix scans over sorted SSTables with bloom filters, compared to SQLite's indexed lookups which must join the index with the compressed raw payload file.
- **Disk usage**: Comparable at scale. FlowDB stores data as binary key/value records in WAL + SST files; SQLite uses a compact index + zstd-compressed raw payload file. The earlier versions showed FlowDB at 5–6× smaller, but that was due to a WAL truncation bug in FlowDB ≤ 0.1.5 that deleted all WAL segments on every flush (fixed in 0.1.6).
- **Durability**: FlowDB 0.1.6 ensures crash-safe WAL persistence. The background maintenance task automatically flushes the memtable, runs compaction, and garbage-collects expired SST files — no manual intervention needed.

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

# Optional: upload captured RTP media as WAV after each call (default: true).
# Set to false to skip media upload (e.g. only signalling is needed).
# media = true

# Optional: also upload SIP signalling as JSONL after each call (default: false).
# signaling = true
```

### HTTP

```toml
[sipflow.upload]
type = "http"
url = "https://archive.example.com/upload"
# headers = { "Authorization" = "Bearer token" }

# Optional: upload captured RTP media as WAV after each call (default: true).
# Set to false to skip media upload (e.g. only signalling is needed).
# media = true

# Optional: also upload SIP signalling as JSONL after each call (default: false).
# signaling = true
```

### Media Upload (`media`)

The `media` option controls whether the captured RTP audio is mixed down to WAV and uploaded after each call. It defaults to `true`, so simply configuring `[sipflow.upload]` is enough to get WAV files uploaded alongside the CDR.

Set `media = false` when you only need SIP signalling uploaded (combine with `signaling = true`) or when another subsystem is responsible for media storage. When `media = false`:

- No WAV is generated or transferred.
- The call record's `recording_url` is **not** pre-populated (the S3 URL pre-fill step is also skipped).
- `signaling` uploads are unaffected.

### Signalling Upload (`signaling`)

When `signaling = true` is set, RustPBX will additionally upload the SIP signalling flow for each completed call as a JSONL file alongside the WAV recording. The JSONL file is uploaded to the same storage target (S3 or HTTP) with the key path `{date_prefix}/{call_id}.jsonl`.

#### JSONL Format

Each line in the JSONL file is a JSON object representing a single SIP/RTP packet captured during the call. The schema:

```json
{
  "timestamp": 1746000000,
  "seq": 1,
  "leg": 0,
  "msg_type": "Sip",
  "src_addr": "192.168.1.10:5060",
  "dst_addr": "192.168.1.20:5060",
  "payload": [73, 78, 86, ...]
}
```

| Field | Type | Description |
|---|---|---|
| `timestamp` | `u64` | Unix epoch (seconds) when the packet was captured |
| `seq` | `u64` | Sequence number within the call (default `0`) |
| `leg` | `Option<i32>` | Call leg index (`0` = caller side, `1` = callee side); omitted when `null` |
| `msg_type` | `string` | `"Sip"` for SIP signalling packets, `"Rtp"` for RTP media packets |
| `src_addr` | `string` | Source IP:port of the packet |
| `dst_addr` | `string` | Destination IP:port of the packet |
| `payload` | `bytes` | Raw packet payload. For SIP messages this is the full SIP message (UTF-8 text). For RTP packets this is the RTP frame bytes |

For HTTP uploads, the JSONL file is sent as a `multipart/form-data` request with the part name `"signaling"` and content type `application/jsonl`. The file name follows the pattern `{call_id}.jsonl`.

---

## Enabling SipFlow

SipFlow capture is enabled by configuring the `[sipflow]` backend. It is independent from `[callrecord]`, which only controls CDR JSON storage.

```toml
# Default configuration (recommended):
# - SipFlow captures SIP signalling AND RTP audio
# - No [recording] section needed
# - FlowDB engine (default): high throughput, compact storage
[sipflow]
type = "local"
root = "./config/sipflow"
engine = "flowdb"
```

When SipFlow is active and **no `[recording]` section exists**, each CDR entry in the console shows both the **SIP Flow** tab (signalling ladder) and the **audio player** (recorded RTP). If a `[recording]` section is present, the audio player relies on the legacy recorder's WAV output instead.

---

## Storage Layout

### FlowDB engine (`engine = "flowdb"`)

FlowDB stores all data in a single directory as LSM-tree files (WAL, SSTables, and indexes):

```
./config/sipflow/
├── WAL/                 # write-ahead log segments (crash-safe, fsync'd on flush)
├── SST/                 # sorted string tables (compacted data)
└── INDEX/               # sparse key indexes
```

Key encoding:
- SIP messages: `sip:{call_id}:{counter:020}`
- RTP packets: `rtp:{call_id}:{leg}:{counter:020}`

Value encoding (binary, no JSON overhead):
- SIP: `[src_len:u16][src_addr][dst_len:u16][dst_addr][payload_bytes]`
- RTP: `[leg:i32][src_len:u16][src_addr][payload_bytes]`

When `ttl_secs` is configured, records carry an `expire_at` timestamp and FlowDB automatically garbage-collects expired entries during compaction.

The background maintenance task (started automatically by the FlowDB backend) periodically:
- Flushes the active memtable to SSTable (every `flush_interval_ms`, default 1s)
- Merges SST files via compaction when count exceeds threshold
- Garbage-collects fully-expired SST files (every `gc_interval_secs`, default 1h)

WAL segments are truncated only after the corresponding memtable has been safely persisted to an SST file, ensuring no data loss on crash.

### SQLite engine (`engine = "sqlite"`)

With `subdirs = "daily"`, files are organised as:

```
./config/sipflow/
└── 20260417/
    ├── 20260417.sqlite   # index: Call-ID → (timestamp, byte offset, size)
    └── 20260417.raw      # payload: SIP messages + RTP frames (zstd compressed)
```

### Cleanup

Older directories (SQLite) or the entire data directory (FlowDB with TTL) can be cleaned up without affecting running calls:

```sh
# SQLite: remove old date directories
find /data/sipflow -maxdepth 1 -type d -name '[0-9]*' \
  -mtime +7 -exec rm -rf {} +

# FlowDB: set ttl_secs in config for automatic expiry,
# or periodically delete the entire data directory and restart.
```

---

→ Next: back to [00-overview.md](00-overview.md)
