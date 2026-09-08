# SipFlow — SIP 信令与 RTP 录音子系统

> 译文说明：完整保留英文原文的结构、示例和基准数据；代码及注释原样保留。原文在“FlowDB 默认”与“engine 默认 sqlite”等处存在不一致，本次不擅自统一默认值。

## 概述

SipFlow 是 rustpbx 中的 SIP/RTP 数据包采集与录音子系统。支持两种本地存储引擎（旧版 SQLite 和 FlowDB LSM-tree）以及远程集群模式，提供信令回放、从 RTP 导出 WAV，以及媒体质量统计。

---

## 架构

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

**模块：**

| 模块 | 路径 | 说明 |
|--------|------|-------------|
| `sipflow/mod.rs` | `crates/rustpbx-sipflow/src/mod.rs` | 核心类型：`SipFlowItem`、`SipFlowMsgType`、`SipFlowMediaStats`、`SipFlowQuery` |
| `sipflow/backend/mod.rs` | `crates/rustpbx-sipflow/src/backend/mod.rs` | `SipFlowBackend` trait 与 `create_backend()` 工厂 |
| `sipflow/backend/local.rs` | `crates/rustpbx-sipflow/src/backend/local.rs` | 本地 SQLite 后端（后台工作任务 + StorageManager） |
| `sipflow/backend/remote.rs` | `crates/rustpbx-sipflow/src/backend/remote.rs` | 远程集群后端（UDP + HTTP，跳跃一致性哈希） |
| `sipflow/storage.rs` | `crates/rustpbx-sipflow/src/storage.rs` | SQLite + 原始文件存储管理器 |
| `sipflow/flowdb_backend.rs` | `crates/rustpbx-sipflow/src/flowdb_backend.rs` | FlowDB 后端实现 |
| `sipflow/flowdb_codec.rs` | `crates/rustpbx-sipflow/src/flowdb_codec.rs` | FlowDB 键值编码 |
| `sipflow/protocol.rs` | `crates/rustpbx-sipflow/src/protocol.rs` | UDP 传输的二进制线协议 |
| `sipflow/wav_utils.rs` | `crates/rustpbx-sipflow/src/wav_utils.rs` | RTP → WAV 生成与编解码转码 |
| `sipflow/rtp_stats.rs` | `crates/rustpbx-sipflow/src/rtp_stats.rs` | RTP 抖动/丢包统计 |
| `sipflow/sdp_utils.rs` | `crates/rustpbx-sipflow/src/sdp_utils.rs` | SDP 解析辅助功能 |
| `bin/sipflow.rs` | `src/bin/sipflow.rs` | 独立 sipflow 服务器二进制 |
| `sipflow/diag.rs` | `crates/rustpbx-sipflow/src/diag.rs` | 共享诊断逻辑（CLI + HTTP `/diag` 端点） |
| `callrecord/sipflow.rs` | `src/callrecord/sipflow.rs` | SIP 消息检查器（MessageInspector），包含批量写入器和对象池 |
| `callrecord/sipflow_upload.rs` | `src/callrecord/sipflow_upload.rs` | S3/HTTP 上传钩子 |
| `console/handlers/sipflow.rs` | `src/console/handlers/sipflow.rs` | Console REST API 端点 |

---

## 数据流路径

### 1. 内嵌模式（callrecord 集成）

```
SIP message / RTP sample
    → SipFlow::inspect_message() / SipFlow::record_rtp()
    → crossbeam channel (BATCH_SIZE=256, flush interval 50ms)
    → SipFlowBackend::record()
    → StorageManager / FlowDB
```

### 2. 独立服务器模式

```
UDP packet → parse_packet() → mpsc channel → convert_packet_to_item()
    → SipFlowBackend::record()
    → SQLite (sipflow.db + data.raw) or FlowDB
    → HTTP API: /flow, /media, /health
```

### 3. 远程集群模式（RemoteBackend）

```
rustpbx node → UDP send to sipflow cluster
    → Jump Consistent Hash node selection
    → Cluster node stores data + HTTP query API
```

---

## 存储后端对比

| 特性 | SQLite（旧版） | FlowDB（默认） |
|---------|----------------|-------------------|
| 存储布局 | sipflow.db（元数据）+ data.raw（载荷） | 单目录 LSM-tree |
| 写入方式 | 批量 INSERT + 事务提交 | 逐记录 `write_batch_sync` |
| 压缩 | 逐包 zstd（级别 3，≥96 字节） | LSM 块级压缩 |
| TTL | 不自动过期 | 内置 TTL 垃圾回收 |
| 子目录策略 | 无 / 按日 / 按小时 | 无 / 按日 / 按小时 |
| 数据隔离 | 通过 `call_meta` 表 JOIN | 键前缀扫描（`sip:{id}:`、`rtp:{id}:`） |

### SQLite 存储格式

**sipflow.db 表结构：**

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

**data.raw 记录格式：**

```
Magic (2B, 0x5346) | orig_size (4B) | comp_size (4B) | payload (comp_size bytes)
```

当 `orig_size ≥ 96` 字节时，载荷采用 zstd 压缩。解压后载荷开头的魔数字节 `0x28 0xB5 0x2F 0xFD` 用于标识 zstd 流。

### FlowDB 存储格式

**键设计：**

```
SIP:   sip:{call_id}:{counter}          (20-digit counter ensures ordering)
RTP:   rtp:{call_id}:{leg}:{counter}
```

**值编码：**

```
SIP:  src_len(2B) | src(dynamic) | dst_len(2B) | dst(dynamic) | payload
RTP:  leg(4B) | src_len(2B) | src(dynamic) | payload
```

### 子目录布局（FlowDB 和 SQLite）

两种本地后端都遵循 `subdirs` 设置。数据在 `root` 下按时间桶分目录，使每个存储实例保持较小规模，方便按天/小时清理，并限制单次查询需要打开的工作集。

| 模式 | 布局 | 示例 |
|------|--------|---------|
| `none` | `<root>/` | `/var/sipflow/data/` |
| `daily` | `<root>/YYYYMMDD/` | `/var/sipflow/data/20260702/` |
| `hourly` | `<root>/YYYYMMDD/HH/` | `/var/sipflow/data/20260702/14/` |

- 记录根据写入时的 `Local::now()` 归入时间桶，因此跨越时间桶边界的通话，其数据会分布在两个目录。查询自动访问与请求 `[start, end]` 范围相交的所有桶并合并结果。
- **SQLite** 为每个桶打开新的 `sipflow.db` + `data.raw`。
- **FlowDB** 维护已打开 `Engine` 实例的 LRU 缓存（每桶一个，最多同时 24 个引擎）。缓存满时，最近最少使用的引擎被刷盘并关闭；数据仍可查询，只需按需重新打开。待处理批次不会丢失：只有内存批次已刷入 LSM-tree 的引擎才可被淘汰。

---

## 性能基准

结果来自 `cargo run --release --example sipflow_bench -- --calls 50 --rtp-per-call 1000 --sip-per-call 20`。

**测试环境：**16 核、32 GB 内存，NVMe SSD。

### 写入吞吐量

| 引擎 | 记录数 | 写入时间 | 写入速率 | 磁盘占用 | 信令查询 | 媒体查询 |
|--------|---------|-----------|-----------|-----------|-----------|------------|
| SQLite | 51,000 | 2.59s | 19,678 rec/s | 5,810 KB | 0.3 ms | 0.3 ms |
| FlowDB | 51,000 | 0.20s | 255,661 rec/s | 1,030 KB | 0.1 ms | 0.0 ms |

### 汇总

| 指标 | SQLite | FlowDB | 比率 |
|--------|--------|--------|-------|
| 写入吞吐量 | 19,678 rec/s | 255,661 rec/s | FlowDB 快 13 倍 |
| 磁盘空间 | 5,810 KB | 1,030 KB | FlowDB 占用缩小 5.6 倍 |
| 信令查询延迟 | 0.3 ms | 0.1 ms | FlowDB 快 4.9 倍 |
| 媒体查询延迟 | 0.3 ms | 0.0 ms | FlowDB 更快 |

> **注意：** FlowDB 先在内存中批处理记录，再同步批量写入，实现比 SQLite **高 13 倍的吞吐量**，同时磁盘占用缩小 5.6 倍。默认 `flush_count`（0）和 `flush_interval_secs`（0）禁用应用层缓冲：每条记录立即写入引擎，并通过 FlowDB 的 LSM memtable 立即可查。对于小帧，FlowDB 的 LSM 块级压缩比逐包 zstd 更有效。

### 正确性验证

- SIP 信令数量：SQLite=20 ✓，FlowDB=20 ✓（一致）。
- RTP 包总数：SQLite=1000 ✓，FlowDB=1000 ✓（一致）。
- 隔离性（不存在的通话）：两者均返回空 ✓。

---

## 配置

### TOML 示例

**本地（默认引擎 = sqlite）：**

```toml
[sipflow]
type = "local"
root = "/var/sipflow/data"
engine = "sqlite"          # "sqlite" (default) or "flowdb"
subdirs = "daily"          # "none" / "daily" / "hourly" (default: "daily")
flush_count = 0            # 0=immediate write, no app-level buffering
flush_interval_secs = 0
id_cache_size = 8192       # CallID→row-id LRU cache (SQLite)
compress = true            # zstd compress payloads (SQLite)
compress_level = 6         # zstd compression level 0-9 (SQLite)

# Optional FlowDB settings
# ttl_secs = 86400
# memtable_size_mb = 64
# block_cache_capacity_mb = 128

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

**本地 FlowDB：**

```toml
[sipflow]
type = "local"
root = "/var/sipflow/data"
engine = "flowdb"
subdirs = "daily"          # "none" / "daily" / "hourly"
ttl_secs = 86400
memtable_size_mb = 64
block_cache_capacity_mb = 128
```

**远程集群：**

```toml
[sipflow]
type = "remote"
nodes = [
  { udp = "10.0.0.1:3000", http = "http://10.0.0.1:3001" },
  { udp = "10.0.0.2:3000", http = "http://10.0.0.2:3001" },
]
timeout_secs = 10
channel_capacity = 40000   # UDP receive channel buffer
dns_ttl_secs = 5           # DNS cache TTL for node resolution
mtu = 0                    # UDP MTU; 0 = use OS default
report_interval_secs = 10  # Cluster health report interval
delegate_upload = false    # Delegate S3/HTTP upload to cluster nodes

# Legacy single-node format:
# udp_addr = "127.0.0.1:3000"
# http_addr = "http://127.0.0.1:3001"
```

### 配置字段

| 字段 | 类型 | 默认值 | 适用范围 | 说明 |
|-------|------|---------|-----------|-------------|
| `type` | `"local"` / `"remote"` | 必填 | 两者 | 后端类型 |
| `root` | String | - | local | 数据目录 |
| `engine` | `"sqlite"` / `"flowdb"` | `"sqlite"` | local | 存储引擎 |
| `subdirs` | `"none"` / `"daily"` / `"hourly"` | `"daily"` | local | 按时间桶划分目录 |
| `flush_count` | usize | 0 | local | 刷盘前的批次大小；0 = 立即写入 |
| `flush_interval_secs` | u64 | 0 | local | 最大刷盘间隔（秒）；0 = 无定时器 |
| `id_cache_size` | usize | 8192 | local | CallID→ID LRU 缓存（SQLite） |
| `compress` | bool | `true` | local | 对存储载荷进行 zstd 压缩（SQLite） |
| `compress_level` | u32 | 6 | local | zstd 压缩级别 0–9（SQLite） |
| `ttl_secs` | Option\<u64\> | None | local | FlowDB 记录 TTL（秒） |
| `memtable_size_mb` | usize | 64 | local | FlowDB memtable 大小 |
| `block_cache_capacity_mb` | usize | 128 | local | FlowDB 块缓存容量 |
| `nodes` | Vec\<SipFlowClusterNode\> | [] | remote | 集群节点列表 `{udp, http}` |
| `udp_addr` | Option\<String\> | None | remote | 旧版单节点 UDP 地址 |
| `http_addr` | Option\<String\> | None | remote | 旧版单节点 HTTP 地址 |
| `timeout_secs` | u64 | 10 | remote | HTTP 查询超时 |
| `channel_capacity` | usize | 40000 | remote | UDP 接收通道缓冲大小 |
| `dns_ttl_secs` | u64 | 5 | remote | 节点解析 DNS 缓存 TTL |
| `mtu` | usize | 0 | remote | UDP MTU；0 = 操作系统默认值 |
| `report_interval_secs` | u64 | 10 | remote | 集群健康报告间隔 |
| `delegate_upload` | bool | `false` | remote | 将 S3/HTTP 上传委托给集群节点 |
| `upload` | Option\<SipFlowUploadConfig\> | None | 两者 | S3/HTTP 上传钩子 |

### 上传配置字段

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `type` | `"s3"` / `"http"` | 必填 | 上传后端 |
| S3 字段：`vendor`、`bucket`、`region`、`access_key`、`secret_key`、`endpoint`、`root` | 多种 | 必填 | S3 连接与路径 |
| HTTP 字段：`url` | String | 必填 | HTTP 端点 URL |
| `headers` | Option\<Map\> | None | 自定义 HTTP 头 |
| `signaling` | Option\<bool\> | `false` | 上传 SIP 信令数据 |
| `media` | Option\<bool\> | `true` | 上传 RTP 媒体（WAV） |
| `force_pcm` | Option\<bool\> | `false` | 上传前转码为 PCM |
| `pcm_sample_rate` | Option\<u32\> | 16000 | `force_pcm` 为 true 时的 PCM 采样率 |

### JSONL 导出格式（`export_jsonl`）

信令上传（`signaling = true`）与录音 JSONL 伴随文件（配置 `[recording]` 但无 `[sipflow]` 后端，见 [06-media-recording.md](06-media-recording_zh.md)）使用完全相同的行结构，每行一个 JSON 对象：

```json
{"timestamp":1753000000123456,"seq":0,"leg":null,"msg_type":"Sip","src_addr":"192.168.1.10:5060","dst_addr":"","payload":"INVITE sip:1001@pbx SIP/2.0\r\n..."}
```

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `timestamp` | u64 | Unix 微秒时间戳 |
| `seq` | u64 | 采集序号 |
| `leg` | null / i32 | 媒体腿（SIP 消息为 null） |
| `msg_type` | string | `"Sip"` 或 `"Rtp"`（`SipFlowMsgType` 的变体名） |
| `src_addr` / `dst_addr` | string | 传输地址（按方向填入其中一侧） |
| `payload` | string | 完整 SIP 消息文本 |

---

## WAV 生成

`wav_utils.rs` 模块从采集的 RTP 数据包重建 WAV 音频：

- 从 SIP SDP 协商中自动检测编解码器。
- 混合通话腿合并（两条腿在一个 WAV 中）或按腿下载。
- **编解码支持：**

| RTP PT | 编解码器 | WAV 格式 | 转码 |
|--------|-------|-----------|-------------|
| 0 | PCMU | PCMU（tag=7） | 否 |
| 8 | PCMA | PCMA（tag=6） | 否 |
| 9 | G722 | L16 16kHz | 是 |
| 18 | G729 | L16 8kHz | 是 |
| 101 | telephone-event | DTMF | 重新生成 |
| 动态 | Opus | L16 48kHz | 是 |

---

## REST API

### Console API（内嵌）

| 方法 | 路径 | 说明 |
|--------|------|-------------|
| GET | `/sipflow/settings` | 获取当前配置 |
| PUT | `/sipflow/settings` | 更新配置 |
| GET | `/sipflow/flow/{call_id}` | 查询 SIP 信令流 |
| GET | `/sipflow/media/{call_id}` | 查询媒体（WAV 下载） |

### 独立服务器 API

| 方法 | 路径 | 参数 | 说明 |
|--------|------|-----------|-------------|
| GET | `/health` | - | 健康检查 |
| GET | `/flow` | `callid`、`start`、`end` | SIP 信令流（JSON） |
| GET | `/media` | `callid`、`start`、`end`、`stats` | 媒体 WAV 或统计 |
| GET | `/diag` | `callid`、`start`、`end` | 综合诊断报告（SIP + RTP + 跨腿分析） |

- `start` / `end`：自动检测格式，支持 Unix 时间戳（秒/微秒）、`YYYYMMDD`、`YYYYMMDDHH`、`YYYYMMDDHH24MISS` 或 `@<unix-ts>`。默认范围为当前时间前后各 1 小时。

---

## 线协议（UDP）

用于独立 sipflow 服务器的 UDP 接收器与 RemoteBackend 传输。

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

## 独立服务器

```bash
# Start with SQLite engine (default)
cargo run --release --bin sipflow -- \
    --port 3000 --http-port 3001 \
    --root /var/sipflow/data

# Start with FlowDB engine
cargo run --release --bin sipflow -- \
    --engine flowdb \
    --port 3000 --http-port 3001 \
    --root /var/sipflow/data \
    --ttl-secs 86400 \
    --memtable-size-mb 256 \
    --block-cache-capacity-mb 512


```

### CLI 选项

| 选项 | 默认值 | 说明 |
|--------|---------|-------------|
| `-a`、`--addr` | `0.0.0.0` | UDP 绑定地址 |
| `-p`、`--port` | `3000` | UDP 接收端口 |
| `--http-port` | `3001` | HTTP 查询端口 |
| `-r`、`--root` | `./config/sipflow` | 数据存储目录 |
| `--engine` | `sqlite` | 存储引擎：`sqlite` 或 `flowdb` |
| `--no-compress` | `false` | 禁用载荷 zstd 压缩（SQLite） |
| `--compress-level` | `6` | zstd 压缩级别 0–9（SQLite） |
| `--subdirs` | `daily` | 目录布局：`none`、`daily`、`hourly` |
| `--buffer-size` | `100000` | 通道缓冲容量 |
| `--recv-buffer-size` | `8388608` | UDP 接收缓冲大小（SO_RCVBUF） |
| `--recv-tasks` | `0` | 并行 UDP 接收任务数（0 = CPU 数量） |
| `--flush-count` | `1000` | 刷盘批次大小（SQLite） |
| `--flush-interval` | `5` | 最大刷盘间隔（秒，SQLite） |
| `--id-cache-size` | `8192` | CallID→ID 缓存大小（SQLite） |
| `--ttl-secs` | 无 | FlowDB 记录 TTL（0 = 不过期） |
| `--memtable-size-mb` | `64` | FlowDB memtable 大小 |
| `--block-cache-capacity-mb` | `128` | FlowDB 块缓存容量 |
| `--log-file` | `/var/log/sipflow.log` | 日志文件路径 |
| `--log-level` | `info` | 日志级别（trace、debug、info、warn、error） |

---

## RTP 质量统计

`query_media_stats()` 按（通话腿、来源、SSRC）返回统计：

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `leg` | i32 | 媒体腿（0=A，1=B） |
| `src` | String | 来源地址 |
| `packet_count` | usize | 已接收包数 |
| `lost_packets` | u64 | 丢包数 |
| `expected_packets` | u64 | 预期包数（= 已接收 + 丢失） |
| `loss_percent` | f64 | 丢包百分比 |
| `jitter_ms` | Option\<f64\> | 抖动，单位为毫秒 |
| `ssrc` | Option\<u32\> | RTP SSRC |
| `payload_type` | Option\<u8\> | RTP 载荷类型 |
| `clock_rate` | Option\<u32\> | 时钟频率 |

---
