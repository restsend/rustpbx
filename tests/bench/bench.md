# RustPBX P2P Benchmark

A comprehensive benchmark tool for testing RustPBX peer-to-peer (P2P) call performance using [sipbot](https://github.com/restsend/sipbot).

## Overview

This benchmark simulates high-load extension-to-extension (P2P) calls:

- **UAS Registration**: Multiple sipbot instances register as extension users (bob/alice) with the PBX
- **UAC Load Generation**: sipbot batch mode (`--total`/`--cps`) generates concurrent calls
- **Resource Monitoring**: Real-time monitoring of PBX CPU, memory, and concurrent calls
- **Media Quality**: Setup latency, RTT, packet loss, TX/RX packet counts from sipbot Progress output
- **Scenario Testing**: Tests 3 different configurations:
  1. `mediaproxy=none` - No media proxying (direct media)
  2. `mediaproxy=all` - All media proxied through PBX
  3. `mediaproxy=all` + `sipflow` enabled - With SIP flow recording

## Requirements

- Python 3.8+
- sipbot 0.2.28+ (with batch mode `--total`/`--cps` and audio loop fix)
- RustPBX compiled (`target/release/rustpbx`)
- `config.toml.dev` configuration file with extension users (bob/alice)

### System Tuning

```bash
sysctl -w net.core.rmem_max=16777216
sysctl -w net.core.wmem_max=16777216
sysctl -w net.core.rmem_default=262144
sysctl -w net.core.wmem_default=262144
```

## Usage

### Basic Usage

Run 500-concurrent benchmark (all scenarios):
```bash
python tests/bench/bench.py --scenario all
```

Run specific scenario:
```bash
python tests/bench/bench.py --scenario mediaproxy_none
python tests/bench/bench.py --scenario mediaproxy_all
python tests/bench/bench.py --scenario sipflow
```

### Advanced Options

```bash
# 800 concurrent
python tests/bench/bench.py --scenario all --total 800 --cps 200 --uas-count 4

# Custom load
python tests/bench/bench.py --scenario mediaproxy_all --total 500 --cps 100 --duration 60

# Quick test
python tests/bench/bench.py --scenario mediaproxy_none --total 50 --cps 10 --duration 10
```

### Command-Line Options

| Option | Default | Description |
|--------|---------|-------------|
| `--scenario` | `all` | Scenario: `mediaproxy_none`, `mediaproxy_all`, `sipflow`, `all` |
| `--total` | 500 | Total number of calls |
| `--cps` | 100 | Calls per second (fast ramp for true concurrency) |
| `--duration` | 60 | Call duration in seconds |
| `--uas-count` | 5 | Number of UAS instances (registered as bob/alice) |
| `--uas-base-port` | 5090 | Base port for UAS instances |
| `--proxy-host` | 127.0.0.1 | SIP proxy host |
| `--proxy-port` | 15061 | SIP proxy port |
| `--http-base` | http://127.0.0.1:8083 | HTTP base URL for health checks |
| `--log-dir` | tests/bench/results | Directory for logs and results |
| `--rustpbx-bin` | target/release/rustpbx | Path to rustpbx binary |
| `--rustpbx-config` | config.toml.dev | Path to rustpbx config |
| `--cooldown` | 10 | Cooldown between scenarios (seconds) |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Test Setup                               │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐       ┌──────────┐   │
│  │  UAS-1   │  │  UAS-2   │  │  UAS-3   │  ...  │  UAS-N   │   │
│  │ bob:5090 │  │ alice:5091│ │ bob:5092 │       │ alice:N  │   │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘       └────┬─────┘   │
│       │             │             │                  │          │
│       └─────────────┴─────────────┴──────────────────┘          │
│                         │ REGISTER (bob/alice)                   │
│                         ▼                                        │
│  ┌────────────────────────────────────────────┐                 │
│  │              RustPBX Proxy                  │                 │
│  │           (mediaproxy setting)              │                 │
│  └────────────────────────────────────────────┘                 │
│                         ▲                                        │
│                         │ INVITE sip:bob (batch --total 500)     │
│                         │ UAC registers as alice                  │
│  ┌──────────────────────┴───────────────────┐                   │
│  │         sipbot call (batch mode)          │                   │
│  │     --total 500 --cps 100 --hangup 60     │                   │
│  └──────────────────────────────────────────┘                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Concurrency Model

The benchmark uses a **fast-ramp + long-hold** model for true simultaneous concurrency:

```
sustained_concurrent = total
sustained_time = call_duration - (total / cps)
```

For 500 concurrent (default): `sustained_time = 60 - 500/100 = 55 seconds`

### Test Flow

1. **Setup Phase**:
   - Generate modified config.toml with specified mediaproxy/sipflow settings
   - Start rustpbx with the test configuration
   - Verify rustpbx health via `/ami/v1/health`

2. **UAS Registration**:
   - Start N sipbot UAS instances on ports 5090, 5091, ...
   - Each registers as bob or alice (cycling)
   - UAS uses `--echo` mode for bidirectional RTP

3. **UAC Load Generation**:
   - Single sipbot batch call: `sipbot call --total 500 --cps 100`
   - UAC registers as alice, calls bob through the PBX
   - PBX routes INVITE to registered bob UAS instances

4. **Monitoring**:
   - ResourceMonitor polls `ps` for CPU/memory
   - Health endpoint polled for concurrent call count
   - sipbot Progress lines parsed for metrics

5. **Result Collection**:
   - Parse sipbot Progress output (setup latency, RTT, loss, TX/RX)
   - Summarize resource usage from monitor samples

## Output Files

### results.jsonl
JSON Lines format with detailed results per scenario.

### results.csv
CSV format for easy import into spreadsheets.

### Log Files
Per-scenario log files in the log directory:
- `rustpbx_*.log` - RustPBX console output
- `sipbot_uas_*.log` - sipbot UAS output
- `uac_batch_*.log` - sipbot UAC batch output

## Metrics Explained

### Call Statistics
| Metric | Description |
|--------|-------------|
| `calls_completed` | Number of calls that completed successfully |
| `calls_failed` | Number of calls that failed |
| `success_rate` | Percentage of successful calls |
| `status_counts` | SIP response code distribution (e.g., 200:497, 487:3) |

### Media Quality
| Metric | Description |
|--------|-------------|
| `avg_setup_latency_ms` | Average call setup latency |
| `avg_rtt_ms` | Average RTCP Round Trip Time |
| `avg_loss_pct` | Average packet loss percentage |
| `max_loss_pct` | Maximum packet loss observed |
| `tx_packets` | Total RTP packets transmitted |
| `rx_packets` | Total RTP packets received |

### Resource Usage
| Metric | Description |
|--------|-------------|
| `cpu_avg` | Average CPU usage during test |
| `cpu_peak` | Peak CPU usage during test |
| `mem_avg_mb` | Average memory usage in MB |
| `mem_peak_mb` | Peak memory usage in MB |
| `calls_peak` | Peak concurrent calls (from health endpoint) |
| `calls_avg` | Average concurrent calls |

## Test Environment

| Item | Value |
|------|-------|
| Date | 2026-04-03 |
| RustPBX | 0.4.0 (release) |
| sipbot | 0.2.28 |
| OS | Linux 5.15.0-118-generic (x86_64) |
| CPU | 16 cores |
| Memory | 32 GB |
| Network | Local loopback (127.0.0.1) |
| Codec | G.711 PCMU |
| UAS Count | 5 (bob × 3, alice × 2) |
| recording.enabled | **false** (disabled to allow true mediaproxy=none bypass) |

---

## Benchmark Results

### 500 Concurrent

**Config:** CPS=100, total=500, duration=60s, 5 UAS. Sustained 55s at full concurrency.

| Metric | mediaproxy=none | mediaproxy=all | mediaproxy=all + sipflow |
|--------|:---:|:---:|:---:|
| **Call Completion** | **500/500 (100%)** | **500/500 (100%)** | **500/500 (100%)** |
| Status Codes | 200:499, 487:1 | 200:495, 487:5 | 200:500 |
| Avg Setup Latency | 4.40 ms | 3.73 ms | 5.96 ms |
| Packet Loss | 0.00% | 0.00% | 0.00% |
| TX Packets (UAC) | 1,497,000 | 1,484,958 | 1,500,000 |
| RX Packets (UAC) | 0* | 742,448 | 749,954 |
| Per-call TX Rate | 49.9 pkt/s | 49.5 pkt/s | 50.0 pkt/s |

> *RX=0 for mediaproxy=none confirms RTP flows directly between endpoints, bypassing PBX entirely.

| Resource | none (Avg) | none (Peak) | all (Avg) | all (Peak) | sipflow (Avg) | sipflow (Peak) |
|----------|:---:|:---:|:---:|:---:|:---:|:---:|
| CPU | 26.0% | **32.4%** | 79.6% | **98.4%** | 84.5% | **101.0%** |
| Memory (RSS) | 125.1 MB | **137.3 MB** | 169.3 MB | **183.1 MB** | 177.6 MB | **198.3 MB** |
| Concurrent Calls | 475.8 | **500** | 475.7 | **500** | 475.7 | **500** |

### 800 Concurrent

**Config:** CPS=200, total=800, duration=60s, 5 UAS. Sustained 56s at full concurrency.

| Metric | mediaproxy=none | mediaproxy=all | mediaproxy=all + sipflow |
|--------|:---:|:---:|:---:|
| **Call Completion** | **800/800 (100%)** | **800/800 (100%)** | **800/800 (100%)** |
| Status Codes | 200:796, 487:4 | 200:796, 487:4 | 200:796, 487:4 |
| Avg Setup Latency | 8.32 ms | 6.38 ms | 6.08 ms |
| Packet Loss | 0.00% | 0.00% | 0.00% |
| TX Packets (UAC) | 2,387,981 | 2,388,000 | 2,388,000 |
| RX Packets (UAC) | 0* | 1,193,943 | 1,193,995 |
| Per-call TX Rate | 49.7 pkt/s | 49.8 pkt/s | 49.8 pkt/s |

> *RX=0 for mediaproxy=none confirms RTP flows directly between endpoints, bypassing PBX entirely.

| Resource | none (Avg) | none (Peak) | all (Avg) | all (Peak) | sipflow (Avg) | sipflow (Peak) |
|----------|:---:|:---:|:---:|:---:|:---:|:---:|
| CPU | 38.4% | **47.9%** | 126.5% | **155.0%** | 128.8% | **156.0%** |
| Memory (RSS) | 173.3 MB | **191.8 MB** | 244.3 MB | **264.8 MB** | 251.6 MB | **280.3 MB** |
| Concurrent Calls | 765.0 | **800** | 765.4 | **800** | 765.4 | **800** |

### 2026-08-13 Regression / Optimization Runs (refactor_media)

Same-machine apples-to-apples (main@940178e7 baseline vs refactor_media HEAD, 1 fork/call,
`--uas-count 2`, no jemalloc). The refactor's media-layer rewrite started with a measurable
CPU/memory regression; the fixes below brought it **below baseline**.

> CPU figures throughout this doc are **per-core** (`ps %CPU`, 100% = one logical core,
> 16 cores available). e.g. 93.6% ≈ 0.94 cores.

| Metric | baseline main | refactor (pre-fix) | refactor (post-fix) |
|--------|:---:|:---:|:---:|
| **800 conc CPU avg** | 95.0% (~0.95 core) | 106.1% (~1.06 cores) | **93.6% (~0.94 core)** |
| **800 conc Mem peak** | 452.2 MB | 473.6 MB | **433.4 MB** |
| **Setup latency** | 17.36 ms | 17 ms | **6.42 ms** (2.7× faster) |
| Success / Loss | 100% / 0% | 100% / 0% | 100% / 0% |

**Key optimizations (all in `crates/rustpbx-media/`):**
- **Dialog leak fix** (`sip_session.rs`): parallel-fork loser legs that got a 2xx before
  being cancelled left their confirmed dialog in `dialog_layer` with no guard → removed +
  hung up on all exit paths.
- **Egress sender ring** `sample_track(Audio, 500→8)` / `(Video, 100→8)`: the ring only needs
  to absorb producer/consumer jitter (1 frame / 20 ms); 500 slots pre-allocated ~95 KB/leg
  (~150 MB at 1600 legs).
- **`rtp_buffer_capacity` 500→100**: ICE pre-ready buffer, idle in steady state with DropOldest.
- **IngressTap fastpath**: packed `(ssrc, pt)` atomic cache skips the per-packet DashMap write;
  lock-free DTMF PT bitmask; `has_recorder` flag skips the recorder RwLock when nothing records.
  SSRC/PT tracking disabled for plain RTP↔RTP bridges (no RTCP PLI/NACK relay needed).
- **Egress pacing parked while relay/gated**: a `pending()` future replaces the 20 ms
  interval tick when no frames are produced (relay fastpath or pre-answer silence),
  eliminating 50 Hz × 1600 legs of empty wakeups.
- **RTP-timeout wire_leg monitor parked when unarmed**: a `Notify` wakes it only on
  arm/resume instead of a fixed 100 ms poll.
- **Rewrite-bridge cycle leak fixed** (`leg.rs`): in the relay fastpath leg A's RtpTransport
  holds a `RewriteBridge` targeting leg B's transport (and vice versa). `PeerConnection::close`
  does not clear the bridge, so the two `Arc<RtpTransport>`s kept each other alive — every
  call leaked ~16 KB. `LegInner::stop`/`Drop` now call `pc.clear_rtp_rewrite_bridge()` first.

### WebRTC ↔ RTP Cross-Transport Stress

New scenarios (`--scenario webrtc_to_rtp*`): the **caller (alice) uses WebRTC (DTLS-SRTP)** and the
**callee (bob) uses plain RTP**. Only alice is marked `is_support_webrtc = true` in the config.
Both legs negotiate PCMU → the PBX uses the fast-path rewrite relay with WebRTC→RTP extension
stripping; differing codecs (opus↔pcmu) exercise the transcode path.

| Scenario | Codecs | Conc | Completion | Loss | CPU avg / peak (per-core) | Mem peak | Audio fmt | Continuity |
|----------|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| `webrtc_to_rtp` | pcmu↔pcmu (relay) | 100 | 100% | 0% | 16.1% / 21.8% (~0.16/0.22 core) | 109 MB | PASS | PASS |
| `webrtc_to_rtp` | pcmu↔pcmu (relay) | 500 | 100% (497/500) | 0% | 71.2% / 82.4% (~0.71/0.82 core) | 328 MB | PASS | PASS |
| `webrtc_to_rtp` | pcmu↔pcmu (relay) | 800 | 100% | 0% | 119.1% / 139.0% (~1.19/1.39 cores) | 484 MB | PASS | PASS |
| `webrtc_to_rtp_transcode` | opus↔pcmu (transcode) | 100 | 100% | 0% | 36.8% / 51.3% (~0.37/0.51 core) | 145 MB | PASS | PASS |
| `webrtc_to_rtp_transcode` | opus↔pcmu (transcode) | 500 | 100% (494/500) | 0% | 212.1% / 264.0% (~2.12/2.64 cores) | 508 MB | PASS | PASS |
| `webrtc_to_rtp_transcode` | opus↔pcmu (transcode) | 500 | 100% (2nd run) | 0% | 216.7% / 268.0% (~2.17/2.68 cores) | 504 MB | PASS | PASS |

Notes:
- **0% packet loss at all loads**, even at ~2.1 cores (212% per-core CPU) during opus↔pcmu
  transcoding. `media_continuity` (seq/ts-jump) PASS confirms no audio glitches.
- Cross-transport relay is confirmed by `strip_a_to_b=true strip_b_to_a=false` in the
  `fast-path relay activated` logs (WebRTC→RTP direction strips extensions).
- Transcode is confirmed by `transcoding activated a_codec=Opus b_codec=PCMU`.
- Opus encode dominates the transcode CPU (perf: `CeltEncoder::encode_impl` 13.96%,
  `Resampler::resample` 4.79%, `pvq` 2.8–3.7%) — expected cost, not a bottleneck.
- `tx_packets=0` in transcode results is a sipbot reporting quirk (sipbot counts TX
  differently when the UAC/UAS codecs differ); `rx_packets` still validates media flow.

#### Transcode vs Relay — Memory

| Metric | Relay (pcmu↔pcmu) | Transcode (opus↔pcmu) | Δ |
|--------|:---:|:---:|:---:|
| **Mem / call** (500 conc, minus ~45 MB idle) | ~0.57 MB | ~0.93 MB | **1.6×** |
| CPU / call | 0.142% | 0.424% | **3.0×** |
| Mem peak (500 conc) | 328 MB | 508 MB | +180 MB |

Transcode memory is only ~1.6× relay — far smaller than the 3× CPU gap. The extra is
**fixed per-leg objects**, not per-packet cost:
- decoder (Opus decode state, ~30–60 KB/leg)
- resampler: `coeffs` (256 phases × 24 taps × 4 B ≈ 24 KB) + history buffer
- one encoder per leg (present in both modes)

Scaling estimate (idle ≈ 45 MB):

```
relay:     Mem ≈ 45 + conc × 0.57 MB   → 16 GB ≈ 28,000 conc (theoretical)
transcode: Mem ≈ 45 + conc × 0.93 MB   → 16 GB ≈ 17,000 conc (theoretical)
```

> **Bottleneck takeaway**: transcode is CPU-bound (3× relay), not memory-bound (1.6× relay).
> Size CPU at ~0.42% per-core / call and memory at ~0.93 MB/call for cross-codec calls;
> same-codec calls relay at ~0.14% per-core / call / ~0.57 MB/call.
>
> Per-core % → cores: divide by 100 (e.g. 0.42%/call × 500 calls ≈ 2.1 cores; 16 cores
> ≈ ~3,800 concurrent transcoded calls).

### 30-minute Memory-Leak Verification (18000 calls, 90 batches)

`--memleak --total 18000 --batch-size 200 --cps 200 --duration 15 --uas-count 2`:

| Metric | Before rewrite-bridge fix | After rewrite-bridge fix |
|--------|:---:|:---:|
| Final RSS | 752.4 MB (+707.5 MB) | **199.0 MB (+154.1 MB)** |
| RSS slope | +6.70 MB/batch (linear leak) | **+0.44 MB/batch (plateau)** |
| Final dialogs | 12 | 3 |
| tasks.total | 6 (reclaimed) | 6 |
| Verdict | "NO LEAK" (task-based, false negative) | **NO LEAK** (actual) |

The baseline (main@940178e7) plateaus at ~186 MB under the same load; the refactor now
matches that after the rewrite-bridge cycle fix.

### Full Comparison

| Level | Scenario | Completion | Peak Conc | Loss | Setup Latency | CPU Peak | CPU Avg | Mem Peak |
|-------|----------|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **500** | none | 100% | **500** | 0.00% | 4.40ms | **32.4%** | 26.0% | 137.3 MB |
| **500** | all | 100% | **500** | 0.00% | 3.73ms | **98.4%** | 79.6% | 183.1 MB |
| **500** | all+sf | 100% | **500** | 0.00% | 5.96ms | **101.0%** | 84.5% | 198.3 MB |
| **800** | none | 100% | **800** | 0.00% | 8.32ms | **47.9%** | 38.4% | 191.8 MB |
| **800** | all | 100% | **800** | 0.00% | 6.38ms | **155.0%** | 126.5% | 264.8 MB |
| **800** | all+sf | 100% | **800** | 0.00% | 6.08ms | **156.0%** | 128.8% | 280.3 MB |

### Per-Channel Overhead

| Metric | 500 (none) | 500 (all) | 500 (sipflow) | 800 (none) | 800 (all) | 800 (sipflow) |
|--------|:---:|:---:|:---:|:---:|:---:|:---:|
| CPU (Peak) | **0.065%** | **0.197%** | **0.202%** | **0.060%** | **0.194%** | **0.195%** |
| Memory (Peak) | **0.275 MB** | **0.366 MB** | **0.397 MB** | **0.240 MB** | **0.331 MB** | **0.350 MB** |

### Media Proxy Overhead Summary

The CPU overhead of mediaproxy=all is **~3×** that of mediaproxy=none, primarily due to ForwardingTrack RTP forwarding:

| Level | none CPU Peak | all CPU Peak | Delta | Ratio |
|-------|:---:|:---:|:---:|:---:|
| 500 | 32.4% | 98.4% | +66.0% | 3.0× |
| 800 | 47.9% | 155.0% | +107.1% | 3.2× |

SipFlow adds minimal overhead (~1-3% CPU), mainly from SQLite SIP signaling writes.

### Resource Scaling Estimate

```
mediaproxy=none (signaling only):
  CPU%    ≈ 8 + concurrent × 0.05
  Mem(MB) ≈ 60 + concurrent × 0.16

  1000 conc: CPU ≈ 58% (0.6 cores),  Mem ≈ 220 MB
  2000 conc: CPU ≈ 108% (1.1 cores), Mem ≈ 380 MB
  5000 conc: CPU ≈ 258% (2.6 cores), Mem ≈ 860 MB

mediaproxy=all (RTP forwarding via ForwardingTrack):
  CPU%    ≈ 8 + concurrent × 0.19
  Mem(MB) ≈ 80 + concurrent × 0.23

  1000 conc: CPU ≈ 198% (2.0 cores), Mem ≈ 310 MB
  2000 conc: CPU ≈ 388% (3.9 cores), Mem ≈ 540 MB
  5000 conc: CPU ≈ 958% (9.6 cores), Mem ≈ 1230 MB
```

### Concurrency Verification

The test polls `/ami/v1/health` -> `sipserver.calls` every second. Peak concurrent matches target exactly.

| Level | Peak Concurrent | Avg Concurrent | Sustained Time |
|-------|:---:|:---:|:---:|
| 500 | **500** | 475.8 | 55s |
| 800 | **800** | 765.2 | 56s |

### Media Throughput

| Level | Scenario | TX Packets | Per-call Rate | Proxy Throughput |
|-------|----------|:---:|:---:|:---:|
| 500 | none | ~1,497K | ~49.9 pkt/s | N/A (bypass) |
| 500 | all | ~1,485K | ~49.5 pkt/s | ~50K pkt/s |
| 500 | all+sf | ~1,500K | ~50.0 pkt/s | ~50K pkt/s |
| 800 | none | ~2,388K | ~49.7 pkt/s | N/A (bypass) |
| 800 | all | ~2,388K | ~49.8 pkt/s | ~80K pkt/s |
| 800 | all+sf | ~2,388K | ~49.8 pkt/s | ~80K pkt/s |

---

## Scenarios

### mediaproxy=none
Direct media path between endpoints. The PBX handles signaling only.

### mediaproxy=all
All RTP traffic flows through the PBX media proxy (ForwardingTrack).

### mediaproxy=all + sipflow
With SIP flow recording enabled for all SIP signaling.

### webrtc_to_rtp / webrtc_to_rtp_rec / webrtc_to_rtp_sipflow
WebRTC → RTP cross-transport call. The caller (alice) registers as a WebRTC endpoint
(`is_support_webrtc = true`, DTLS-SRTP media) while the callee (bob) stays plain RTP. The PBX
bridges the two transports: same negotiated codec → fast-path rewrite relay (WebRTC→RTP strips
extension headers); recording / sipflow variants suffix the name.

### webrtc_to_rtp_transcode / _rec / _sipflow
Same cross-transport WebRTC → RTP setup, but the caller negotiates **opus** while the callee
uses **pcmu** — forcing the decode → resample → re-encode transcode path. Exercises Opus
encoding and the 48 kHz ↔ 8 kHz resampler at scale.

```bash
# Quick smoke (100 concurrent)
python tests/bench/bench.py --scenario webrtc_to_rtp --total 100 --cps 20 --duration 15 --uas-count 2

# 500 concurrent relay
python tests/bench/bench.py --scenario webrtc_to_rtp --total 500 --cps 100 --duration 60 --uas-count 2

# 500 concurrent opus↔pcmu transcode
python tests/bench/bench.py --scenario webrtc_to_rtp_transcode --total 500 --cps 100 --duration 60 --uas-count 2
```

## Troubleshooting

### Port conflicts
```bash
# Kill stale sipbot processes
pkill -9 -f "sipbot.*127.0.0.1:509"
```

### RustPBX health check fails
```bash
ps aux | grep rustpbx
tail -f tests/bench/results/rustpbx_*.log
curl http://127.0.0.1:8083/ami/v1/health
```

### High packet loss
```bash
ulimit -n 65535
sysctl -w net.core.rmem_max=16777216
```

---

*Part of the RustPBX project.*
