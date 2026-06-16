# SipFlow — SIP Signaling & RTP Recording Subsystem

## Overview

SipFlow is the SIP/RTP packet capture and recording subsystem in rustpbx. It supports two local storage engines (SQLite legacy and FlowDB LSM-tree) and a remote cluster mode, providing signaling replay, WAV export from RTP, and media quality statistics.

---

## Architecture

```
SIP / RTP data flow
    │
    ▼
┌─────────────────┐     ┌──────────────────┐
│  callrecord/    │────▶│  SipFlowBackend  │
│  sipflow.rs     │     │  (trait)         │
│  (MessageInsp.) │     └────────┬─────────┘
└─────────────────┘              │
                    ┌────────────┼────────────┐
                    ▼            ▼            ▼
             ┌──────────┐ ┌──────────┐ ┌──────────┐
             │  Local   │ │  Local   │ │  Remote  │
             │ (Sqlite) │ │ (FlowDB) │ │ (UDP+HTTP)│
             └──────────┘ └──────────┘ └──────────┘
```

**Modules:**

| Module | Path | Description |
|--------|------|-------------|
| `sipflow/mod.rs` | `src/sipflow/mod.rs` | Core types: `SipFlowItem`, `SipFlowMsgType`, `SipFlowMediaStats`, `SipFlowQuery` |
| `sipflow/backend/mod.rs` | `src/sipflow/backend/mod.rs` | `SipFlowBackend` trait + `create_backend()` factory |
| `sipflow/backend/local.rs` | `src/sipflow/backend/local.rs` | Local SQLite backend (background worker + StorageManager) |
| `sipflow/backend/remote.rs` | `src/sipflow/backend/remote.rs` | Remote cluster backend (UDP + HTTP, jump consistent hash) |
| `sipflow/storage.rs` | `src/sipflow/storage.rs` | SQLite + raw-file storage manager |
| `sipflow/flowdb_backend.rs` | `src/sipflow/flowdb_backend.rs` | FlowDB backend implementation |
| `sipflow/flowdb_codec.rs` | `src/sipflow/flowdb_codec.rs` | FlowDB key/value encoding |
| `sipflow/protocol.rs` | `src/sipflow/protocol.rs` | Binary wire protocol for UDP transport |
| `sipflow/wav_utils.rs` | `src/sipflow/wav_utils.rs` | RTP → WAV generation with codec transcoding |
| `sipflow/rtp_stats.rs` | `src/sipflow/rtp_stats.rs` | RTP jitter/loss statistics |
| `sipflow/sdp_utils.rs` | `src/sipflow/sdp_utils.rs` | SDP parsing helpers |
| `bin/sipflow.rs` | `src/bin/sipflow.rs` | Standalone sipflow server binary |
| `callrecord/sipflow.rs` | `src/callrecord/sipflow.rs` | SIP message inspector (MessageInspector) with batch writer + object pool |
| `callrecord/sipflow_upload.rs` | `src/callrecord/sipflow_upload.rs` | S3/HTTP upload hooks |
| `console/handlers/sipflow.rs` | `src/console/handlers/sipflow.rs` | Console REST API endpoints |

---

## Data Flow Paths

### 1. Embedded Mode (callrecord integration)

```
SIP message / RTP sample
    → SipFlow::inspect_message() / SipFlow::record_rtp()
    → crossbeam channel (BATCH_SIZE=256, flush interval 50ms)
    → SipFlowBackend::record()
    → StorageManager / FlowDB
```

### 2. Standalone Server Mode

```
UDP packet → parse_packet() → mpsc channel → convert_packet_to_item()
    → SipFlowBackend::record()
    → SQLite (sipflow.db + data.raw) or FlowDB
    → HTTP API: /flow, /media, /health
```

### 3. Remote Cluster Mode (RemoteBackend)

```
rustpbx node → UDP send to sipflow cluster
    → Jump Consistent Hash node selection
    → Cluster node stores data + HTTP query API
```

---

## Storage Backend Comparison

| Feature | SQLite (legacy) | FlowDB (default) |
|---------|----------------|-------------------|
| Storage layout | sipflow.db (metadata) + data.raw (payloads) | LSM-tree single directory |
| Write method | Batch INSERT + transaction commit | Per-record `write_batch_sync` |
| Compression | Per-packet zstd (level 3, ≥96 bytes) | Block-level LSM compression |
| TTL | No auto-expiry | Built-in TTL garbage collection |
| Subdirectory policy | None / Daily / Hourly | Single directory |
| Data isolation | JOIN via `call_meta` table | Key prefix scan (`sip:{id}:`, `rtp:{id}:`) |

### SQLite Storage Format

**sipflow.db schema:**

```sql
CREATE TABLE call_meta (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    callid TEXT UNIQUE NOT NULL
);
CREATE INDEX idx_callid ON call_meta(callid);

CREATE TABLE sip_msgs (
    id INTEGER PRIMARY KEY,
    call_id INTEGER NOT NULL,
    src TEXT NOT NULL, dst TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    offset INTEGER NOT NULL, size INTEGER NOT NULL
);
CREATE INDEX idx_sip_call ON sip_msgs(call_id);

CREATE TABLE media_msgs (
    id INTEGER PRIMARY KEY,
    call_id INTEGER NOT NULL, leg INTEGER NOT NULL,
    src TEXT NOT NULL DEFAULT '',
    timestamp INTEGER NOT NULL,
    offset INTEGER NOT NULL, size INTEGER NOT NULL
);
CREATE INDEX idx_media_call ON media_msgs(call_id);
CREATE INDEX idx_media_call_timestamp ON media_msgs(call_id, timestamp);
```

**data.raw record format:**
```
Magic (2B, 0x5346) | orig_size (4B) | comp_size (4B) | payload (comp_size bytes)
```

The payload is zstd-compressed when `orig_size ≥ 96` bytes. The magic bytes `0x28 0xB5 0x2F 0xFD` at the start of a decompressed payload identify zstd streams.

### FlowDB Storage Format

**Key design:**

```
SIP:   sip:{call_id}:{counter}          (20-digit counter ensures ordering)
RTP:   rtp:{call_id}:{leg}:{counter}
```

**Value encoding:**

```
SIP:  src_len(2B) | src(dynamic) | dst_len(2B) | dst(dynamic) | payload
RTP:  leg(4B) | src_len(2B) | src(dynamic) | payload
```

---

## Performance Benchmark

Results from `cargo run --release --example sipflow_bench -- --calls 50 --rtp-per-call 1000 --sip-per-call 20`

**Test environment:** 16C32G, NVMe SSD

### Write Throughput

| Engine | Records | Write Time | Write Rate | Disk Usage | Flow Query | Media Query |
|--------|---------|-----------|-----------|-----------|-----------|------------|
| SQLite | 51,000 | 2.59s | 19,678 rec/s | 5,810 KB | 0.3 ms | 0.3 ms |
| FlowDB | 51,000 | 0.20s | 255,661 rec/s | 1,030 KB | 0.1 ms | 0.0 ms |

### Summary

| Metric | SQLite | FlowDB | Ratio |
|--------|--------|--------|-------|
| Write throughput | 19,678 rec/s | 255,661 rec/s | FlowDB 13× faster |
| Disk space | 5,810 KB | 1,030 KB | FlowDB 5.6× smaller |
| Flow query latency | 0.3 ms | 0.1 ms | FlowDB 4.9× faster |
| Media query latency | 0.3 ms | 0.0 ms | FlowDB faster |

> **Note:** FlowDB batches records in memory before performing a synchronous batch write, achieving **13× higher throughput** than SQLite while using 5.6× less disk space. The default `flush_count` (0) and `flush_interval_secs` (0) disable app-level buffering — each record is written to the engine immediately and is instantly queryable via FlowDB's LSM memtable. FlowDB's block-level LSM compression is more effective than per-packet zstd on small frames.

### Correctness Verification

- SIP flow count: SQLite=20 ✓, FlowDB=20 ✓ (MATCH)
- RTP packet sum: SQLite=1000 ✓, FlowDB=1000 ✓ (MATCH)
- Isolation (non-existent call): both return empty ✓

---

## Configuration

### TOML Examples

**Local FlowDB (default):**

```toml
[sipflow]
type = "local"
root = "/var/sipflow/data"
engine = "flowdb"          # "flowdb" (default) or "sqlite"
ttl_secs = 86400           # optional TTL
memtable_size_mb = 64
block_cache_capacity_mb = 128

# Optional S3/HTTP upload hook
[sipflow.upload]
type = "s3"
vendor = "aws"
bucket = "sipflow-recordings"
region = "us-east-1"
access_key = "..."
secret_key = "..."
endpoint = "https://s3.amazonaws.com"
root = "recordings/"
```

**Local SQLite:**

```toml
[sipflow]
type = "local"
root = "/var/sipflow/data"
engine = "sqlite"
subdirs = "daily"          # "none" / "daily" / "hourly" (SQLite only)
# flush_count = 0          # 0=immediate write, no app-level buffering
# flush_interval_secs = 0
id_cache_size = 8192
```

**Remote cluster:**

```toml
[sipflow]
type = "remote"
nodes = [
  { udp = "10.0.0.1:3000", http = "http://10.0.0.1:3001" },
  { udp = "10.0.0.2:3000", http = "http://10.0.0.2:3001" },
]
timeout_secs = 10

# Legacy single-node format:
# udp_addr = "127.0.0.1:3000"
# http_addr = "http://127.0.0.1:3001"
```

### Configuration Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | `"local"` / `"remote"` | required | Backend type |
| `root` | String | - | Data directory (local) |
| `engine` | `"flowdb"` / `"sqlite"` | `"flowdb"` | Storage engine (local) |
| `subdirs` | `"none"` / `"daily"` / `"hourly"` | `"daily"` | Directory partitioning (local SQLite) |
| `flush_count` | usize | 0 | Batch size before flush; 0 = immediate write (local) |
| `flush_interval_secs` | u64 | 0 | Max flush interval; 0 = no timer (local) |
| `id_cache_size` | usize | 8192 | CallID→ID LRU cache (local SQLite) |
| `ttl_secs` | Option\<u64\> | None | FlowDB TTL (local FlowDB) |
| `memtable_size_mb` | usize | 64 | FlowDB memtable size (local FlowDB) |
| `block_cache_capacity_mb` | usize | 128 | FlowDB block cache (local FlowDB) |
| `nodes` | Vec\<SipFlowClusterNode\> | [] | Cluster node list (remote) |
| `timeout_secs` | u64 | 10 | HTTP query timeout (remote) |
| `upload` | Option\<SipFlowUploadConfig\> | None | S3/HTTP upload hook |

---

## WAV Generation

The `wav_utils.rs` module reconstructs WAV audio from captured RTP packets:

- Automatic codec detection from SIP SDP negotiation
- Mixed-leg merge (both legs in one WAV) or per-leg download
- **Codec support:**

| RTP PT | Codec | WAV Format | Transcoding |
|--------|-------|-----------|-------------|
| 0 | PCMU | PCMU (tag=7) | No |
| 8 | PCMA | PCMA (tag=6) | No |
| 9 | G722 | L16 16kHz | Yes |
| 18 | G729 | L16 8kHz | Yes |
| 101 | telephone-event | DTMF | Regenerated |
| Dynamic | Opus | L16 48kHz | Yes |

---

## REST API

### Console API (embedded)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/sipflow/settings` | Get current config |
| PUT | `/sipflow/settings` | Update config |
| GET | `/sipflow/flow/{call_id}` | Query SIP signaling flow |
| GET | `/sipflow/media/{call_id}` | Query media (WAV download) |

### Standalone Server API

| Method | Path | Parameters | Description |
|--------|------|-----------|-------------|
| GET | `/health` | - | Health check |
| GET | `/flow` | `callid`, `start`, `end` | SIP flow (JSON) |
| GET | `/media` | `callid`, `start`, `end`, `stats` | Media WAV or stats |

- `start` / `end`: Unix timestamp (seconds), defaults to ±1 hour from now
- `stats=1`: Return RTP statistics instead of audio file

---

## Wire Protocol (UDP)

Used by standalone sipflow server's UDP receiver and RemoteBackend transport.

```
offset  size  field
0       1     version (currently 1)
1       1     msg_type (0=SIP, 1=RTP)
2       2     src_port (big endian)
4       16    src_ip  (IPv6 address, IPv4 uses ::ffff:x.x.x.x)
20      2     dst_port
22      16    dst_ip
38      8     timestamp (microseconds)
46      4     call_id_len
50      N     call_id  (UTF-8)
50+N    1     leg_len (0=no leg, 1=leg in next byte, otherwise has leg)
51+N    M     leg value (when leg_len=1, 1 byte)
52+N    K     payload
```

---

## Standalone Server

```bash
# Start with FlowDB engine (default)
cargo run --release --bin sipflow -- \
    --port 3000 --http-port 3001 \
    --root /var/sipflow/data

# Start with SQLite engine
cargo run --release --bin sipflow -- \
    --engine sqlite \
    --port 3000 --http-port 3001 \
    --root /var/sipflow/data \
    --flush-count 1000 --flush-interval 5

# Start with FlowDB + TTL + large memtable
cargo run --release --bin sipflow -- \
    --engine flowdb \
    --port 3000 --http-port 3001 \
    --root /var/sipflow/data \
    --ttl-secs 86400 \
    --memtable-size-mb 256 \
    --block-cache-capacity-mb 512
```

### CLI Options

| Option | Default | Description |
|--------|---------|-------------|
| `-a`, `--addr` | `0.0.0.0` | UDP bind address |
| `-p`, `--port` | `3000` | UDP receive port |
| `--http-port` | `3001` | HTTP query port |
| `-r`, `--root` | `./config/sipflow` | Data storage directory |
| `--engine` | `flowdb` | Storage engine: `flowdb` or `sqlite` |
| `--buffer-size` | `100000` | Channel buffer capacity |
| `--flush-count` | `1000` | Flush batch size (SQLite) |
| `--flush-interval` | `5` | Max flush interval in seconds (SQLite) |
| `--id-cache-size` | `8192` | CallID→ID cache size (SQLite) |
| `--ttl-secs` | none | FlowDB record TTL (0 = no expiry) |
| `--memtable-size-mb` | `64` | FlowDB memtable size |
| `--block-cache-capacity-mb` | `128` | FlowDB block cache capacity |

---

## RTP Quality Statistics

`query_media_stats()` returns per-(leg, source, SSRC) statistics:

| Field | Type | Description |
|-------|------|-------------|
| `leg` | i32 | Media leg (0=A, 1=B) |
| `src` | String | Source address |
| `packet_count` | usize | Received packets |
| `lost_packets` | u64 | Lost packets |
| `expected_packets` | u64 | Expected packets (= received + lost) |
| `loss_percent` | f64 | Packet loss percentage |
| `jitter_ms` | Option\<f64\> | Jitter in milliseconds |
| `ssrc` | Option\<u32\> | RTP SSRC |
| `payload_type` | Option\<u8\> | RTP payload type |
| `clock_rate` | Option\<u32\> | Clock rate |
