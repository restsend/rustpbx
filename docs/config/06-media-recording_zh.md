# 媒体、录音与 CDR

> 译者注：本篇保留英文原文的全部示例及说明。原文关于录音优先级、SipFlow 和 CDR 默认表的描述与其他文档存在版本差异；本次修复翻译完整性，不将这些描述视为已通过实现验证。

## 媒体代理

控制 RTP 流量的处理方式，可在两个层级配置：

### 服务器级（`[proxy]`）

设置所有通话的默认值。存在中继级配置时，由中继级配置覆盖。

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

### 中继级覆盖（`[proxy.trunks.<name>]`）

对通过此中继路由的通话，覆盖服务器级 `media_proxy`。适用于不同中继需要不同媒体处理方式的情况，例如覆盖网络上的呼叫终结，或强制将容易受 NAT 影响的端点媒体锚定在 PBX。

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

### Latching（源地址学习）

`enable_latching = true`（默认）时，即使被叫 SDP 公布的是局域网私有 IP，PBX 也会从第一个收到的数据包学习被叫实际 RTP 地址。这使 `media_proxy = "auto"` 或 `"force_transcode"` 能正确处理 NAT 后的端点，无需显式改写 SDP。

**服务器级默认值：**

```toml
[proxy]
enable_latching = true
latching_probation_max_packets = 6
```

**中继级覆盖：**

```toml
[proxy.trunks.example]
enable_latching = false
```

### 选择合适的组合（Bug 1 + 2 场景）

| 场景 | 服务器 `media_proxy` | 中继 `media_mode` | 中继 `external_ip` | 结果 |
|----------|---------------------|--------------------|--------------------|--------|
| 普通 SIP 中继（公网） | `auto`（默认） | — | — | 直接呼叫旁路，应用/队列呼叫锚定 |
| 覆盖网络中继 | `auto` | `force_transcode` | `100.64.x.x` | 媒体锚定在 PBX，SDP 使用覆盖网络 IP |
| 出站中继（被叫在 NAT 后） | `auto` | `force_transcode` | — | 锚定并通过 latching 处理 NAT |
| 分机直接通话 | `auto` | — | — | 旁路，由 SIP 自然处理 NAT |

### 仅中继的 WebRTC 腿（`relay_only`）

拨号计划级开关（`media.relay_only`，默认 `false`）。启用后，该通话的
WebRTC 腿只广播、只检查 TURN **中继（relay）** 候选：SDP 应答中不再包含
host 与 server-reflexive 候选，对端只能通过 TURN 分配通道到达 PBX。

#### 适用场景：EIP NAT（1:1 NAT）故障

PBX 位于 1:1 NAT（如云厂商弹性公网 IP）后时，WebRTC 主叫腿会出现特定故障：

- “浏览器 srflx ↔ PBX host（NAT 后）”候选对**能通过单个 STUN binding
  check**，浏览器（ICE controlling）据此提名，呼叫正常接通。
- 随后的 **DTLS 握手报文无法到达 PBX**——NAT/防火墙对 PBX 未先外发注册的
  流放行小体积 STUN，但拦截其后的 DTLS 载荷。
- 结果：SIP `200 OK` 已接通，约 30 秒后 `PC state New → Failed`，主叫腿
  零媒体、无录音。而同一坐席**应答** PBX 发起的呼叫一切正常——因为该方向
  由 PBX（ICE controlling）先发包，已注册 NAT 流。

设置 `relay_only = true` 将 DTLS 迁移到 `浏览器 → TURN → PBX` 路径，完全
绕开该 NAT。

```toml
# 拨号计划媒体配置（路由 media 段 / API 拨号计划）
[media]
relay_only = true
# 需要含 turn: 的 ICE 服务器（服务器 [rtp] ice_servers 或拨号计划
# ice_servers）——没有 TURN 服务器时 relay-only 无可用候选
```

**运维注意**

- relay-only 通话的媒体全部经过 TURN：按话务量规划 coturn（带宽 ≈ 每通话
  2 × 音频码率）并监控；TURN 不可用时 relay-only 通话无法建立媒体。
- 回退即时：`relay_only = false`（配置热加载）后恢复标准 RFC 5245 行为。
- 该类失败在 CDR 可见：应答后某腿零媒体结束的通话会标记错误码
  `proxy.leg_media_incomplete`（见通话记录 `metadata.error_code` /
  trace 事件 `media_issue`）。
- 相关 ICE 选项：`prefer_srflx_over_natted_host`（服务端作为 controlling
  时的检查排序）。

**诊断**（rustrtc ≥ 0.3.133）：debug 日志 `ClientHello decoded`（对端
套件）、`Buffering out-of-order handshake message` /
`Handshake message reassembled`，以及
`Received DTLS packet but no receiver registered — dropped` 计数，可将
失败精确定位到浏览器侧 / 接收竞态 / 网络丢包。

## 录音策略

> **对于 RTP 采集，[recording] 和 [sipflow] 互斥。**
> 默认配置使用 `[sipflow]` 同时采集 SIP 信令和 RTP 音频。只有明确需要旧版实时 WAV 录音器时才配置 `[recording]`。详见 [08-sipflow.md](08-sipflow_zh.md)。

控制何时录音。可在顶层 `[recording]` 或代理级 `[proxy.recording]` 设置（代理级覆盖顶层）。

`[recording]` 控制实时 WAV 录音器。启用时，录音器总是先写入本地 WAV。设置 `type = "http"` 或 `type = "s3"` 只会在通话结束后上传该本地 WAV。

录音配置优先于 SipFlow RTP 录音。如果配置了顶层 `[recording]` 或代理级 `[proxy.recording]`，它就拥有录音决策权：

- `enabled = true`：使用实时 WAV 录音器以及可选的 `[recording]` 上传。
- `enabled = false`：不录制 RTP 媒体。
- 启用 `[sipflow]` 时仍然采集 SipFlow SIP 消息。
- 对本通通话禁用 SipFlow RTP 采集以及 `[sipflow.upload]` 录音导出。

只有在希望由 SipFlow 采集 RTP 音频和/或让 `[sipflow.upload]` 作为录音来源时，才完全省略录音节。

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

# SIP signalling JSONL sidecar (written next to the WAV file). Default true.
# Without a [sipflow] backend, every recorded call also writes a lightweight
# {session_id}.jsonl (same schema as SipFlow export_jsonl) capturing the SIP
# messages of the call. It is uploaded together with the WAV when
# type = "http"|"s3", and powers the console "SIP flow" view/download.
# Set signaling = false to record WAV only, with no JSONL sidecar or upload.
# signaling = true

# Or configure per-proxy
[proxy.recording]
enabled = true
directions = ["inbound"]
auto_start = true
```

### HTTP 录音上传

```toml
[recording]
enabled = true
auto_start = true
type = "http"
path = "./recordings"
url = "https://archive.example.com/recording"
# headers = { "Authorization" = "Bearer token" }
```

### S3 录音上传

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

使用 `[recording] type = "http"` 或 `type = "s3"` 时，CDR 可能在媒体上传结束前写入。上传成功后更新数据库中的 `recording_url`。本地 CDR JSON 在 `recordingUrl` 中保留本地录音路径，在 `recorder[]` 中保留录音器元数据。

### 按需分段录音

每个媒体 leg 都带有录音捕获通道，任何通话都可以**通话中**随时开始/停止录音——即使 `[recording] enabled = false`、路由策略未命中也是如此。用于"只录 IVR 阶段"、"坐席接通后才录"等分阶段录音场景，无需开启整通话录音。

触发方式：

- IVR 节点 `record_start` / `record_stop`（见 IVR Step 协议文档）
- RWI `record.start` / `record.stop`（`call.originate` 也支持内联 `record`）
- Console / CTI HTTP API（`POST /calls/active/{session_id}/commands`、`POST /cc/calls/{call_id}/record`）
- 对话内 SIP INFO（`application/vnd.rustpbx+json`，`{"action": "record.start", "params": {...}}`）

自动生成的分段文件名为 `{path}/{root_session_id}_{seq}_{label}.wav`：

- `seq` — 本通通话内从 1 递增的录音序号；目标文件已存在时自动顺延（同一逻辑通话的不同 leg——如转接前后——共享根 session id）。
- `label` — 启动时解析：显式 `label` 参数 → 会话扩展 `resolved_agent_id` / `agent_id`（坐席接通后写入）→ 会话扩展 `ivr`（IVR 阶段写入）→ `segment_type`。

示例：来电在主 IVR 录一段、坐席接通后录一段 → `abc123_1_main-ivr.wav`、`abc123_2_1001.wav`。

每个完成的片段都会写入 CDR（`metadata.recording_segments`，每段含 `seq` / `label` / `segment_type` / `segment_id` / 起止时间）。上传成功后**每个片段各发一条** `recording_metadata_available` RWI/webhook 事件（`filename` / `download_url` / `file_size` 为该段独有，`extra` 在呼叫级元数据之外附带 `seq` / `label` / `segment_type`）；`record_end` 仍保持每通呼叫一条汇总。上传配置与整通话录音一致（`type = "local"|"http"|"s3"`，失败写 `.upload_failed.*` 标记并后台重试）。

呼叫中心坐席段：cc.toml 支持

```toml
[recording]
record_on_agent_connect = true
```

开启后，坐席接通时 CC addon 自动启动一段 `segment_type = "agent"` 的录音（以坐席 id 作为 label）；坐席挂断但客户通话继续时（满意度调查、回 IVR）自动收段。已在录音中的会话（策略整通话录音或前一段未结束）不会被改动。

### SIP 信令 JSONL 伴随文件

使用 `[recording] enabled = true` 且**没有**配置 `[sipflow]` 后端（或使用 `force_file = true`）时，每通录音都会在 WAV 文件旁额外写入 SIP 信令伴随文件：

- 路径：`{path}/{session_id}.jsonl`（额外通话腿为 `{path}/{session_id}_{call_id}.jsonl`）。
- 格式：每行一个 JSON 对象，**与 SipFlow `export_jsonl` 的结构逐字节一致**：

```json
{"timestamp":1753000000123456,"seq":0,"leg":null,"msg_type":"Sip","src_addr":"192.168.1.10:5060","dst_addr":"","payload":"INVITE sip:1001@pbx SIP/2.0\r\n..."}
```

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `timestamp` | u64 | Unix 微秒时间戳 |
| `seq` | u64 | 采集序号 |
| `leg` | null / i32 | 媒体腿（SIP 消息为 null） |
| `msg_type` | string | 始终为 `"Sip"` |
| `src_addr` / `dst_addr` | string | 传输地址（按方向填入其中一侧） |
| `payload` | string | 完整 SIP 消息文本 |

- 上传：`type = "http"` 或 `type = "s3"` 时，JSONL 与 WAV 一起上传（同一存储路径）；仅在上传成功后删除本地文件。失败时写入同目录的 `.upload_failed.{filename}` 标记。
- 控制台：未配置 SipFlow 后端时，CDR 详情页的“SIP flow”查看/下载功能回退到该文件。
- 禁用：在 `[recording]` 中设置 `signaling = false`，完全跳过伴随文件及其上传，仅录制/上传 WAV。

## CDR（通话明细记录）

### 数据库 CDR（始终开启）

每通通话自动持久化到所配置数据库的 `rustpbx_call_records` 表。这是主要的 CDR 机制，无需额外配置；Web 控制台的“Call Records”页面从该表读取。

只要设置了 `database_url`（始终必填），就会写入通话记录。

### 可选 CDR 输出端（`[callrecord]`）

`[callrecord]` 节是**可选的**。它在始终开启的数据库持久化之外增加一个原始 CDR 输出端。如果只需要数据库 CDR（通常如此），可以完全省略该节。

`max_concurrent` 控制可同时运行的通话后 CDR 保存/上传/钩子任务数，默认为 `64`；小于 `1` 的值会被限制为 `1`。

### 数据库

将 CDR JSON 写入单独的数据库表（默认 `call_records`）。

```toml
[callrecord]
type = "database"
# database_url = "sqlite://cdr.sqlite3"     # Optional: separate database
# table_name = "call_records"               # Optional: custom table name
max_concurrent = 64
```

### 可选存储类型

以下示例均为**可选**的附加输出。不论是否使用，上述数据库 CDR 都保持启用。

### 本地文件系统

```toml
[callrecord]
type = "local"
root = "./cdr_archive"
max_concurrent = 64
```

### S3 兼容对象存储

将 CDR JSON 上传到 AWS S3、MinIO、DigitalOcean Spaces 等。

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

将 CDR JSON 发送到指定端点。

```toml
[callrecord]
type = "http"
url = "http://my-crm/cdr-hook"
max_concurrent = 64

# Deprecated and ignored. Recording media upload is configured by [recording].
with_media = true
keep_media_copy = false
```

HTTP CDR 投递使用 `multipart/form-data`，字段名为 `calllog.json`。录音媒体由 `[recording] type = "http"` 单独投递。
