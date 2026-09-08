# SIP 代理核心

> 译文说明：按英文原文完整翻译，代码、配置、协议示例及其注释原样保留。

`[proxy]` 节控制核心 SIP 信令引擎。

## 监听器与传输协议

配置要启用的端口和协议。

```toml
[proxy]
addr = "0.0.0.0"

# Standard SIP ports
udp_port = 5060
# Multiple UDP ports (e.g., for multi-tenant or port range binding)
# udp_ports = [5060, 5062, 5064]
# Omit both udp_port and udp_ports to disable UDP listening.
# Port 0 requests an OS-assigned ephemeral port; it does not disable UDP.
tcp_port = 5060

# Encrypted SIP (SIPS)
tls_port = 5061
# Uses proxy-level ssl_* configs (if not set, falls back to top-level ssl_*)
ssl_certificate = "./certs/fullchain.pem"
ssl_private_key = "./certs/privkey.pem"
# PEM bundle used to validate certificates on outbound SIP/TLS connections
tls_ca_certificates = "/etc/ssl/certs/ca-certificates.crt"

# WebRTC (SIP over WebSocket)
ws_port = 8089
ws_handler = "/ws"

# RWI (RustPBX WebSocket Interface) — real-time call control WebSocket path
rwi_path = "/rwi/v1"

# AMI (Asterisk Manager Interface) HTTP path (default: "/ami/v1")
# ami_path = "/ami/v1"
```

## SIP 身份与行为

```toml
[proxy]
# SIP User-Agent header
useragent = "RustPBX/1.2.0"
# Appended to Call-ID (e.g., id@rustpbx.com)
callid_suffix = "rustpbx.com"
# Allowed domains (realms). Empty = allows all matching IP/Host.
realms = ["example.com", "sip.process-one.net"]
```

### Realm 的作用

Realm 用于划分 SIP 命名空间，并为认证提供上下文。

- **安全**：认证挑战针对特定 Realm 发出。用户必须提供对该 Realm 有效的凭据。
- **域路由**：多个域托管在同一 IP 上时，Realm 使代理能够区分应使用哪些设置或用户数据库。
- **身份**：通常用作 SIP URI 的域部分（例如 `user@realm`）。

可在 **Web Console** 的 **Settings > Proxy Settings** 中管理 Realm。修改后需要重启服务才能完全生效。

## 并发与保护

```toml
[proxy]
# Max simultaneous transactions handled
max_concurrency = 5000

# Reject matching User-Agents
ua_black_list = ["friendly-scanner", "pplsip"]
# Only allow specific User-Agents (if set, others are rejected)
ua_white_list = []

# If true, silently ignore requests to unknown users (anti-scanning)
ensure_user = true

# Frequency limiter (optional format: "100/60s" for 100 requests per 60 seconds)
frequency_limiter = "100/60s"

# SIP Session Timers (RFC 4028)
session_timer = true
# Keep refreshing session timer even if peer does not negotiate it
# session_timer_always = false
session_expires = 1800  # 30 minutes

# RTP timeout — if no audio packets are received on either direction for
# this many seconds, the call is automatically terminated (default: 30)
rtp_timeout = 30

# SIP Transaction Timers (RFC 3261) - optional overrides
t1_timer = 500      # T1 timer in milliseconds (default: 500)
t1x64_timer = 32000 # T1x64 timer in milliseconds (default: 32000)
```

### 工作线程隔离

RustPBX 使用两个专用 Tokio 运行时，分别处理 SIP 信令与 RTP 媒体转发。这可以防止媒体面高负载挤占 SIP 定时器和事务任务，否则高并发下可能出现 408 Request Timeout（例如 `mediaproxy = "all"` 且有数千通电话时）。

```toml
[proxy]
# Number of worker threads for the SIP signalling runtime.
# Default: min(4, num_cpus) — typically 4 on multi-core machines.
sip_worker_threads = 4

# Number of worker threads for the RTP/media runtime.
# Default: num_cpus - sip_worker_threads.
media_worker_threads = 28
```

- **`sip_worker_threads`**：专门处理 SIP 消息解析、事务管理和定时器。信令较轻量，大多数部署使用 2–4 个线程即可。
- **`media_worker_threads`**：专门处理 RTP/WebRTC 媒体转发、SRTP 加解密、转码、文件播放与录音 I/O。应为其分配大部分可用 CPU 核心。

> **注意**：这些设置只在启动时应用，修改需要重启。两个运行时完全独立，繁忙的媒体运行时不会阻塞 SIP 信令运行时。

## 模块

控制内部逻辑处理流水线的加载。

```toml
[proxy]
# Default stack
modules = ["acl", "auth", "registrar", "call"]

# Enable additional Addons
addons = ["wholesale", "monitor"]
```

## 媒体与编解码器

```toml
[proxy]
# Media proxy mode: auto, all, none, nat, bypass
#   auto:    Bridge RTP only when necessary (e.g., WebRTC ↔ UDP, or NAT detected)
#   all:     Always bridge RTP (B2BUA style)
#   nat:     Bridge only if private IP is detected
#   none:    Direct media (signaling only)
#   bypass:  Rewrite SDP but let RTP flow directly between endpoints
media_proxy = "auto"

# Preferred audio codec order for SDP negotiation
audio_codecs = ["opus", "pcmu", "pcma", "g729"]

# Video codec allowlist for re-INVITEs sent to RTP/PSTN/IMS trunks.
# WebRTC-only codecs (VP8, VP9, AV1, …) and all rtcp-fb feedback attributes
# are stripped automatically. Defaults to ["H264"] when not set.
# video_codecs = ["H264"]

# Codec selection strategy for WebRTC endpoints.
# "performance" (default): avoid transcoding, keep only caller-offered codecs.
# "quality": prefer Opus > G729 > G722 > G711 (may require transcoding).
codec_strategy = "performance"

# Enable NAT media latching (helps with RTP behind NAT, default: true)
enable_latching = true

# Maximum RTP packets to observe during latching probation before committing
# to a candidate source address. Higher values improve stability on flaky
# networks at the cost of slower latch convergence. (default: 6, only used
# when enable_latching = true)
latching_probation_max_packets = 6

# Enable NAT fix for SIP signaling
nat_fix = true
```

## 文件加载与路径

### 文件系统模式（默认）

```toml
[proxy]
# Directory for auto-generated configs (managed by UI/API)
generated_dir = "./config"

# Load additional config files matching these patterns
routes_files = ["config/routes/*.toml"]
trunks_files = ["config/trunks/*.toml"]
acl_files = ["config/acl/*.toml"]
queues_files = ["config/queues/*.toml"]
ivr_files = ["config/ivr/*.toml"]

# Directory for queue-specific data files
queue_dir = "./queues"
```

### 数据库模式

设置 `generated_db = true`，将所有生成配置存入应用数据库而不是文件系统。适用场景包括：

- **高可用部署**：需要在节点间共享配置。
- **Kubernetes/容器环境**：文件系统是临时的。
- **多实例集群**：需要集中管理配置。

```toml
[proxy]
generated_db = true
# generated_dir, routes_files, trunks_files, etc. are ignored
# in DB mode — all generated configs use the config_entries table.
```

此模式下，以下配置类型写入数据库 `config_entries` 表，并从中加载：

| 配置类型 | 分类 | 条目名称 |
|-------------|----------|-----------|
| 中继 | `trunks` | `trunks.generated.toml` |
| 路由 | `routes` | `routes.generated.toml` |
| 队列 | `queue` | `queues.generated.toml` |
| ACL | `acl` | `acl.generated.toml` |
| IVR 项目 | `ivr` | `{name}.generated.toml` |
| CC ACD | `cc_acd` | `acd.toml` |
| CC 技能组 | `cc_skill_groups` | `skill_groups.generated.toml` |
| CC 坐席 | `cc_agents` | `agents.generated.toml` |
| CC 转接 | `cc_transfer` | `transfer.toml` |

> **注意**：模式之间没有迁移路径。从文件系统切换到数据库模式（或反向切换）时，需要通过管理控制台重新导出所有配置。

### 内嵌 ACL 规则

### 内嵌 ACL 规则

ACL 规则也可以与 `acl_files` 一起内嵌定义在 `rustpbx.toml` 中：

```toml
[proxy]
# Inline rules are merged with rules loaded from acl_files
acl_rules = [
    "allow all",
    "deny all",
]
```

## 呼叫处理

```toml
[proxy]
# Registrar default expires time in seconds (default: 30).
# This is the fallback value when the REGISTER request does not include
# an Expires header or Contact expires parameter.
# Both settings can be changed via the Web Console under Settings > Proxy.
registrar_expires = 30

# Maximum allowed expires value in seconds (default: 50).
# Client-requested expires exceeding this limit will be capped.
# Set to a higher value if clients need longer registration lifetimes.
max_registrar_expires = 50

# Passthrough failure status codes to caller
# When true, caller receives the same SIP error code (e.g., 486, 603) from callee
# When false, a generic error code is sent
passthrough_failure = true

# Use SIP REFER for blind transfers (default: false, uses re-INVITE)
# blind_transfer_use_refer = false

# Direct extension-to-extension (P2P) calls: when a callee has multiple
# registered devices, ring ALL of them in parallel and connect the first that
# answers (the remaining forks are cancelled). Default: true.
# Set to false to ring only the most recently registered device instead.
# parallel_fork = true

# Global default max ring/setup time (seconds) before a no-answer call is
# rejected with 408 Request Timeout. 0 or unset disables the ring timeout —
# the call rings until answered or the caller cancels. Per-trunk and per-route
# `max_ring_time` override this global value. Hot-reloadable (new calls only).
# max_ring_time = 60

# Maximum items for SIP flow storage (per dialog)
sip_flow_max_items = 1000
```

## 对话内认证缓存

启用后，成功认证的对话及其来源地址会被缓存。在 TTL 窗口内，同一来源地址发来的后续对话内请求（例如 re-INVITE、BYE）跳过重复认证，以减少延迟和负载。

```toml
[proxy]
# Enabled by default. Set to false to disable.
dialog_auth_cache = { enabled = true, cache_size = 10000, ttl_seconds = 3600 }
```

- **`enabled`**：是否跳过已缓存对话内请求的认证。默认 `true`。
- **`cache_size`**：最大缓存对话数（LRU 淘汰）。默认 `10000`。
- **`ttl_seconds`**：缓存条目的存活时间。默认 `3600`（1 小时）。

显式禁用：

```toml
[proxy]
dialog_auth_cache = { enabled = false }
```

## 通道容量

控制内部异步通道的容量，这些通道用于传递会话和媒体命令/事件。增大容量可能改善高并发表现，但会消耗更多内存；减小容量可能降低背压延迟。

```toml
[proxy]
# Session command channel capacity (default: 256)
session_cmd_channel_capacity = 256
# Session state change channel capacity (default: 256)
session_state_channel_capacity = 256
# Media engine command channel capacity (default: 512)
media_cmd_channel_capacity = 512
# Media engine event channel capacity (default: 1024)
media_event_channel_capacity = 1024
```

- **`session_cmd_channel_capacity`**：每个 SIP 会话最多待处理命令数（例如挂断、转接、播放）。默认 `256`。
- **`session_state_channel_capacity`**：每个会话最多待处理状态变化通知数。默认 `256`。
- **`media_cmd_channel_capacity`**：每个媒体引擎实例最多待处理命令数（例如播放、停止、录音）。默认 `512`。
- **`media_event_channel_capacity`**：每个引擎实例最多待处理媒体事件数（例如 DTMF、播放完成）。默认 `1024`。

## DoS 防护

针对 SIP 流量的限速和防扫描控制。

```toml
[proxy]
# Enable DoS protection
dos_enabled = false

# Max calls per second per source IP (default: 100)
dos_max_cps_per_ip = 100

# Max concurrent dialogs per source IP (default: 500)
dos_max_concurrent_per_ip = 500

# Number of failed probe attempts before blocking (default: 50)
dos_scan_probe_threshold = 50

# Block duration in seconds for detected scanners (default: 600)
dos_scan_block_duration_secs = 600
```

## SBC / 可信代理支持

RustPBX 位于 SIP 代理或 SBC 后方时，socket 层的来源地址始终是代理 IP，会影响 ACL 匹配、中继识别和 DoS 跟踪。使用 `trusted_proxies` 从 Via 头链中提取真实客户端 IP。

```toml
[proxy]
# List of trusted proxy/SBC IPs or CIDR networks.
# When a request arrives from one of these addresses, the real client
# IP is extracted from the Via chain instead of using the socket IP.
# Leave empty (default) if no proxy is in front of PBX.
trusted_proxies = ["10.0.0.1", "10.0.0.0/24"]
```

### 工作原理

1. 如果 socket 层来源 IP **不匹配** `trusted_proxies` 中任何条目，直接使用该 IP（非代理场景）。
2. 如果匹配，PBX 遍历 Via 头链，跳过第一个条目（直接相连的代理），以及后续 sent-by IP 同样匹配 `trusted_proxies` 的条目（多跳 SBC）。
3. 第一个**不在**可信列表中的 Via 条目被视为客户端：
   - 如果有 `received=` 参数（RFC 3581，客户端位于 NAT 后时设置），使用该 IP。
   - 否则使用 Via 的 sent-by IP。
4. 如果所有 Via 条目都属于可信代理，则回退到 socket IP。

### 示例：单个 SBC

```
Client (1.2.3.4) → SBC (10.0.0.1) → PBX

Via: SIP/2.0/UDP 10.0.0.1:5060;branch=sbc
Via: SIP/2.0/UDP 1.2.3.4:5060;received=1.2.3.4;branch=client
```

- Socket IP = 10.0.0.1 → 匹配 `trusted_proxies`。
- 跳过 entry[0]（10.0.0.1，匹配可信代理）。
- entry[1] = 1.2.3.4 → 不可信 → 即客户端。
- 存在 `received=1.2.3.4` → 使用 received IP。

### 示例：无 NAT 的客户端

```
Client (1.2.3.4, no NAT) → SBC (10.0.0.1) → PBX

Via: SIP/2.0/UDP 10.0.0.1:5060;branch=sbc
Via: SIP/2.0/UDP 1.2.3.4:5060;branch=client    (no received=)
```

- 流程相同，但没有 `received=` → 使用 sent-by IP 1.2.3.4。

### 示例：多跳 SBC

```
Client (1.2.3.4) → SBC1 (10.0.0.2) → SBC2 (10.0.0.1) → PBX

Via: SIP/2.0/UDP 10.0.0.1:5060;branch=sbc2
Via: SIP/2.0/UDP 10.0.0.2:5060;branch=sbc1
Via: SIP/2.0/UDP 1.2.3.4:5060;received=1.2.3.4;branch=client
```

- Socket IP = 10.0.0.1 → 匹配可信代理。
- 跳过 entry[0]（10.0.0.1，匹配可信代理）。
- entry[1] = 10.0.0.2 → 匹配可信代理 → 跳过。
- entry[2] = 1.2.3.4 → 不可信 → 即客户端。

### 安全

只有使用防火墙保护 PBX 监听器、确保所有流量必须经过可信 SBC/代理时，才启用 `trusted_proxies`。如果攻击者能够直接向 PBX 发请求，其 IP 不应匹配任何 `trusted_proxies` 条目（将直接使用其 socket IP）。

## URI 校验

```toml
[proxy]
# Maximum URI length accepted (default: 256)
uri_max_length = 256

# Reject malformed URIs (default: false)
uri_reject_malformed = false
```

## 紧急号码

配置紧急呼叫处理以满足当地要求。

```toml
[proxy]
[proxy.emergency]
enabled = true
numbers = ["110", "119", "120", "122", "911", "999"]
emergency_trunk = "pstn-trunk"
```

## 身份与隐私

这些设置控制 PBX 在 SIP 信令和 SDP 媒体属性中如何标识自己。

```toml
[proxy]
# Contact header username when no dialplan caller_contact is set.
# If not specified, a random 16-char hex string is generated at startup.
contact_username = "my-pbx-01"

# CNAME value used in SDP a=ssrc:<n> cname:<value> attributes.
# If not specified, a random 16-char hex string is generated at startup.
# This replaces the default "rustrtc-cname-<ssrc>" in generated SDP.
rtc_cname = "my-pbx-01"
```

两者均未指定时，默认使用同一个随机生成的十六进制字符串，使 PBX 实例可被识别，同时不暴露实现细节。

---

## 会话钩子

会话钩子允许插件观察并影响通话生命周期。通过程序调用 [`SipServerBuilder::with_session_hook`] 注册钩子。

### 内置钩子

| 钩子 | 模块 | 触发条件 | 用途 |
|------|--------|---------|---------|
| `CcCallSessionHook` | `addons::cc` | connected / held / ended / agent_disconnected | CC 生命周期事件 + CDR 推送 |
| `IvrExecHook` | `proxy::proxy_call::ivr_exec_hook` | `on_app_exited` | IVR exec 自动取消保持 + 结果投递 |

### IvrExecHook

`IvrExecHook` 处理 `ivr.exec` 的退出后流程：通过 `ivr.exec` 启动的应用退出时，钩子从会话扩展中读取执行状态，写入结果（或发送 Webhook），并指示会话取消被叫保持、返回结果 SIP INFO。

注册方式（启用 CC 插件时已自动完成）：

```rust
use std::sync::Arc;
use crate::proxy::proxy_call::ivr_exec_hook::IvrExecHook;

SipServerBuilder::new(config)
    .with_session_hook(Arc::new(IvrExecHook))
    // ...
```

完整的 `ivr.exec` 协议参考见 [`docs/ivr_exec.md`](../ivr_exec.md)。
