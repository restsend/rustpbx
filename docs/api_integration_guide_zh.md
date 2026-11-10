# RustPBX API 集成指南

> 译文说明：本篇按英文原文章节完整翻译。代码、配置、协议示例及其中的注释原样保留，便于与原文逐块核对；原文的功能状态描述不代表本次已经完成实现验证。

RustPBX 提供一整套 HTTP API 和 Webhook，使其成为完全可编程的**软件定义 PBX（SD-PBX）**。本指南详细介绍如何将外部业务逻辑（CRM、ERP、AI 助手、计费系统）与 RustPBX 集成。

---

## 🏗️ 架构概览

RustPBX 通过两种方式与外部系统交互：

1. **入站 API（REST）**：你的系统调用 RustPBX，管理资源（分机、中继）或控制活动通话。
2. **出站 Webhook**：RustPBX 调用你的系统，作出路由决策、报告事件或认证用户。

| 机制 | 方向 | 类型 | 用途 |
| :--- | :--- | :--- | :--- |
| **Console API** | 入站 | REST | 分机增删改查、下载录音、系统配置 |
| **活动通话控制** | 入站 | REST | 实时挂断、转接、静音、强制接听 |
| **AMI API** | 入站 | REST | 健康检查、热重载、查看原始对话 |
| **HTTP Router** | 出站 | Webhook | 动态呼叫路由（每个 INVITE 的决策） |
| **用户后端** | 出站 | Webhook | 外部 SIP 认证（OAuth/LDAP 代理） |
| **Locator Webhook** | 出站 | Webhook | 实时注册/注销事件 |
| **通话记录推送** | 出站 | Webhook | 向外部服务器推送 CDR JSON 和音频文件 |

---

## 📡 1. 出站 Webhook（RustPBX → 你的服务器）

### 1.1 HTTP Router（动态呼叫路由）

**这是最强大的扩展点。** RustPBX 可以不使用静态路由规则，而是询问你的 API：“收到了 A 呼叫 B 的请求，我该怎么处理？”

- **触发条件**：每个进入的 SIP INVITE。
- **配置**：

  ```toml
  [proxy.http_router]
  url = "https://your-api.com/pbx/route"
  fallback_to_static = true       # If your API fails, use internal routes
  timeout_ms = 5000
  [proxy.http_router.headers]
  X-API-Key = "secret-token"
  ```

**请求（POST）**：

```json
{
  "call_id": "ab39-551-229",
  "from": "<sip:1001@pbx.com>",
  "to": "<sip:200@pbx.com>",
  "source_addr": "1.2.3.4:5060",
  "source_trunk": "carrier-a",  // 呼入所经的中继；非中继呼入时为 null
  "direction": "inbound",  // inbound | outbound | internal
  "method": "INVITE",
  "uri": "sip:200@pbx.com",
  "headers": {
    "User-Agent": "Yealink T54W",
    "X-Client-ID": "998877"
  },
  "body": "v=0\r\n..." // Full SDP body
}
```

**响应**：

```json
{
  "action": "forward",            // Actions: forward | reject | abort | spam | not_handled
  "targets": [
    "sip:1001@192.168.1.50:5060", // Target 1 (Extension)
    "sip:1002@192.168.1.51:5060"  // Target 2 (Mobile App)
  ],
  "strategy": "parallel",         // parallel (Ring All) | sequential (Failover)
  "record": true,                 // Enable recording for this call
  "record_start_at": "media",    // media (includes early media) | answer (wait for 200)
  "timeout": 30,                  // Ring timeout in seconds
  "media_proxy": "auto",          // all | auto | nat | none | bypass
  "headers": {                    // Inject custom SIP headers into the INVITE sent to B
    "X-Call-Reason": "support-ticket-123"
  }
}
```

`record_start_at` 为本通通话覆盖 `[recording].auto_start_at`。省略时继承全局值；两者均未设置时默认为 `media`。也可以只返回此字段而不返回 `record`，在保留全局录音启用策略的同时覆盖开始时机。

**顺序重试与 RTP 网卡选择**：

```json
{
  "action": "forward",
  "strategy": "sequential",
  "targets": [
    {"uri": "sip:1001@192.0.2.10:5060", "retry_policy": {"status_codes": [408, 503]}},
    {"uri": "sip:1001@192.0.2.11:5060", "retry_policy": {"status_codes": []}}
  ],
  "media_proxy": "all",
  "rtp_config": {"bind_ip": "10.0.0.10", "external_ip": "203.0.113.10"}
}
```

`targets` 可以混用 SIP URI 字符串和包含 `uri`、可选 `retry_policy` 的对象。
重试策略仅在目标级配置；当前目标失败时，只有状态码在其 `status_codes`
列表内才尝试下一个目标。状态码范围为 300–699，空列表禁止继续重试。
字符串目标或未设置策略的对象保留原有行为：失败后继续下一个目标。每个目标只尝试一次，
不会重新请求 HTTP 路由服务。接听或主叫取消后停止；`parallel` 并行呼叫不使用重试策略。
`max_ring_time` 限制整个呼叫建立阶段，`timeout` 限制通话时长。

`rtp_config.bind_ip` 指定本机 RTP/SRTP socket 绑定地址，必须是 PBX 主机上的 IP；
`external_ip` 指定 SDP 宣告地址。两者支持 IPv4/IPv6 字面量，省略或 null 继承服务器配置。
这些设置作用于整个呼叫，重试时保持不变。需要媒体经过指定网卡时使用
`media_proxy: "all"`；直连或 bypass 不会绑定代理 RTP socket。
WebRTC 仍然通过 ICE 跨网卡收集候选地址，不受此固定绑定地址限制。
此对象仅开放上述两个字段，其余 RTC 设置仍采用现有服务器默认值。
无效 IP、未知 `rtp_config` 字段或无效重试状态码会导致 SIP 500 路由错误。
无效目标 URI 继续按原有行为跳过。

**中继目标**：目标可以直接指定已配置的中继名称，其余配置均取自该中继：

```json
{
  "action": "forward",
  "strategy": "sequential",
  "targets": [
    {"trunk": "carrier-a"},
    {"trunk": "carrier-b", "callee": "8613800000000"},
    {"trunk": "carrier-c", "retry_policy": {"status_codes": [503]}}
  ]
}
```

- `trunk`（必填）：中继名称。中继不存在或已禁用时，呼叫以 SIP 500 失败。
- `callee`（可选）：被叫号码，默认为原被叫用户；纯用户名会补全为中继目的地址的主机。
- `retry_policy`（可选）：覆盖中继自身的[重试策略](trunk-retry.md)。两者都没有时，
  该目标失败后不再继续，与其他路由到中继的目标一致。

中继按静态路由规则的方式生效：目的地址与传输协议、认证信息、主机改写、编解码、
网络配置与媒体 IP、`max_ring_time` 以及话单中的中继归属。顺序呼叫中每个中继的
`max_ring_time` 只限制自己的这次尝试；只有一个中继目标且响应未设置 `max_ring_time`
时，它限制整个呼叫。响应中的 `caller` 和 `headers` 同样作用于中继目标。
同一目标不能同时包含 `uri` 和 `trunk`。并行呼叫只使用中继的目的地址和认证信息。

对于经中继呼入的呼叫（`source_trunk`），该中继的网络路径（网络配置、媒体 IP、ICE-lite）
与静态路由一样作用于每个顺序目标；中继目标自身的设置优先，响应中的 `rtp_config`
地址又优先于两者。`rtp_config` 未设置的部分（RTP 端口范围、SIP Contact 地址）仍由网络配置提供。

### 1.2 用户后端（SIP 认证）

将 SIP 注册密码检查委托给外部数据库或 API。

- **触发条件**：带有认证信息的 SIP REGISTER 或 INVITE。
- **配置**：

  ```toml
  [[proxy.user_backends]]
  type = "http"
  url = "https://your-api.com/pbx/auth"
  username_field = "u"
  realm_field = "r"
  ```

**请求（GET）**：`https://your-api.com/pbx/auth?u=1001&r=pbx.com`

**响应（200 OK）**：

```json
{
  "id": 1001,
  "username": "1001",
  "password": "hashed_password_or_plaintext", // HA1 hash preferred
  "display_name": "John Doe",
  "email": "john@pbx.com",
  "allow_guest_calls": false
}
```

**响应（403 Forbidden）**：

```json
{ "reason": "invalid_password", "message": "Account locked" }
```

### 1.3 Locator Webhook（在线状态事件）

在设备上线或离线时实时通知。

- **配置**：

  ```toml
  [proxy.locator_webhook]
  url = "https://your-api.com/pbx/events"
  events = ["registered", "unregistered", "offline"]
  ```

**载荷**：

```json
{
  "event": "registered",
  "timestamp": "2026-02-18T00:20:34+08:00",
  "location": {
    "aor": "sip:1001@1.2.3.4:12345",
    "home_proxy": "10.0.0.12:5060",
    "destination": "TLS 1.2.3.4:12345",
    "supports_webrtc": false,
    "transport": "TLS",
    "user_agent": "MicroSIP/3.21.3",
    "expires": 3600
  }
}
```

`registered` 和 `unregistered` 事件包含一个 `location`。`offline` 事件包含 `locations` 数组，因为传输连接关闭或过期扫描可能一次移除多个绑定。

| 字段 | 含义 |
| --- | --- |
| `event` | `registered`、`unregistered` 或 `offline` |
| `timestamp` | RFC 3339 时间戳，RustPBX 构造 Webhook 载荷时的**服务器本地时区**（如 `+08:00`；宿主未设 `TZ` 时为 `+00:00`） |
| `aor` | 来自 REGISTER 的 `Contact` 头的设备 Contact URI；它不一定是用户的规范身份 |
| `home_proxy` | 接受并持有该注册的 RustPBX 节点所公布的 SIP 地址 |
| `destination` | RustPBX 观察到的设备网络地址，包括可用的传输协议信息 |
| `supports_webrtc` | 是否将该注册设备视为 WebRTC 端点 |
| `transport` | 注册使用的 SIP 传输协议，例如 `UDP`、`TCP`、`TLS`、`WS` 或 `WSS` |
| `user_agent` | REGISTER 中提供的 `User-Agent` 值（如有） |
| `expires` | 注册有效期，单位为秒 |

Locator Webhook 不包含 REGISTER 的 Request-URI。在直接连接的单节点部署中，它可能与 `home_proxy` 地址相同；当客户端通过域名、负载均衡器、SIP 代理或 NAT 注册时，两者可能不同。需要识别持有该注册的 RustPBX 节点时，应使用 `home_proxy`。

`offline` 载荷示例：

```json
{
  "event": "offline",
  "timestamp": "2026-02-18T00:21:34+08:00",
  "locations": [
    {
      "aor": "sip:1001@1.2.3.4:12345",
      "home_proxy": "10.0.0.12:5060",
      "destination": "TLS 1.2.3.4:12345",
      "supports_webrtc": false,
      "transport": "TLS",
      "user_agent": "MicroSIP/3.21.3",
      "expires": 3600
    }
  ]
}
```

### 1.4 CDR 事件推送与录音上传

通话结束后立即推送通话详情。录音媒体上传单独配置。

- **配置**：

  ```toml
  [recording]
  enabled = true
  auto_start = true
  auto_start_at = "media"
  type = "http"
  path = "./config/recorders"
  url = "https://your-api.com/pbx/recording"

  [callrecord]
  type = "http"
  url = "https://your-api.com/pbx/cdr"
  # Maximum concurrent post-call CDR save/upload/hook tasks. Default: 64, minimum: 1.
  max_concurrent = 64
  # Accepted for compatibility, but ignored. Use [recording] for media upload.
  with_media = true
  ```

**CDR 格式**：`multipart/form-data`

- 字段 `calllog.json`：完整 CDR JSON（见下一节）。

**录音格式**：`multipart/form-data`

- 文件字段 `recording`：录制的 WAV 文件。
- 字段 `call_id` 和 `track_id`：录音元数据。

---

## 🔌 2. 入站 REST API（你的系统 → RustPBX）

**基础 URL**：`http://<rustpbx-ip>:8080/console`  
**认证**：会话 Cookie（通过控制台 UI 登录）或静态 API Token：

```toml
[console]
api_tokens = [
  { token = "my-api-token", scopes = ["calls", "records", "routing"] }
]
```

```http
Authorization: Bearer my-api-token
```

### 2.1 活动通话控制

管理正在进行的通话。

**列出活动通话**：

REST 端点挂载在控制台的 `api_prefix` 下（默认为 `/api`，通过 `[console].api_prefix` 配置）。

`GET {api_prefix}/calls/active`

**控制通话**：

`POST {api_prefix}/calls/active/{call_id}/commands`

**载荷**：

1. **挂断**：

   ```json
   { "action": "hangup", "reason": "admin_kick" }
   ```
2. **盲转**：

   ```json
   { "action": "transfer", "target": "sip:1002@pbx.com" }
   ```
3. **静音/取消静音**：

   ```json
   { "action": "mute", "track_id": "audio-0" } // use 'unmute' to reverse
   ```
4. **强制接听**（针对正在振铃的通道）：

   ```json
   { 
     "action": "accept", 
     "sdp": "v=0..." // Server-generated SDP answer
   }
   ```

**会话用户数据（User Data）**：

为活动通话挂载任意业务上下文（CRM 工单号、客户画像等）。值必须是 JSON 对象（≤ 16 KiB），每次写入**全量替换**旧对象。以通话的 `session_id` 为键，并且：

- 自动附加到后续所有 call-scoped RWI 事件 / webhook 的 `user_data` 键下；
- 每次变更发出 `call_userdata_updated` 事件（携带全量新值）；
- 通话结束后写入 CDR `metadata["user_data"]`；
- 集群模式下复制到所有节点（`event_type=user_data`），`PUT/GET` 若命中非归属节点会自动转发到归属节点。

`PUT {api_prefix}/calls/active/{session_id}/userdata`

```json
{ "crm_id": "C-1001", "customer": { "tier": "gold" } }
```

响应：`200` 返回 `{ "message": "User data updated", "data": { ... } }`；通话不存在/已结束返回 `404`；body 不是 JSON 对象返回 `400`；超过 16 KiB 返回 `413`。

`GET {api_prefix}/calls/active/{session_id}/userdata` — 回读当前对象（未设置时为空对象）。

### 2.1.1 实时通话转写（SSE）

订阅外部质检服务返回的实时文字。有效的 `[proxy.quality] enabled = true` 或 wholesale 租户／路由策略会在接听后自动启动质检；SSE 连接和断开只订阅／取消订阅事件，不启停质检。服务返回的 `text` 转发、`calltrace` 记录、`hangup` 执行，无需额外开关。事件网关不可用时返回 `503`。

**端点**：`GET /cc/calls/{call_id}/transcript`（SSE，仅包含文本事件：`started` / `segment` / `error` / `ended`）

同样的文本片段也会作为标准 RWI 事件（`transcript_started` / `transcript_segment` / `transcript_error` / `transcript_ended`）发送给 Webhook / RWI WebSocket 订阅者。

完整协议、配置和示例：[实时转写 SSE API](live_transcript_api.md)。

### 2.2 系统管理（CRUD）

| 资源 | 端点 | 方法 | 说明 |
| :--- | :--- | :--- | :--- |
| **分机** | `{api_prefix}/extensions` | `GET`、`POST`、`PUT`、`DELETE` | 管理 SIP 用户 |
| **中继** | `{api_prefix}/sip-trunk` | `GET`、`POST`、`PUT`、`DELETE` | 管理上游运营商 |
| **路由** | `{api_prefix}/routing` | `GET`、`POST`、`PUT`、`DELETE` | 管理拨号计划规则 |
| **CDR** | `{api_prefix}/call-records` | `GET`、`POST`（搜索） | 查询历史记录 |
| **录音** | `{api_prefix}/call-records/{id}/recording` | `GET` | 流式获取音频文件 |
| **SIP Flow** | `{api_prefix}/call-records/{id}/sip-flow` | `GET` | 获取类似 PCAP 的梯形时序图 JSON |

### 2.3 AMI（管理接口）

底层系统操作，通过 IP 白名单保护（配置中的 `[ami].allows`）。

**基础 URL**：`http://<rustpbx-ip>:8080/ami/v1`

- **健康状态**：`GET /health`——系统关键统计（运行时长、活动通话数、负载）。
- **重载**：`POST /reload/trunks`、`/reload/routes`、`/reload/acl`——不重启地热重载配置。
- **关闭**：`POST /shutdown`——优雅关闭（停止接收新通话，等待活动通话结束）。
- **对话**：`GET /dialogs`——输出内部 SIP 对话的原始状态（用于调试）。
- **SipFlow 信令**：`GET /sipflow/flow/{call_id}`——查询 SIP 梯形时序数据。
- **SipFlow 媒体**：`GET /sipflow/media/{call_id}`——将通话媒体导出为 WAV。

SipFlow 端点支持可选的时间范围查询参数：

- `start`：范围起始时间。
- `end`：范围结束时间。

接受的格式：

- RFC3339 日期时间，例如 `2026-04-16T10:00:00+08:00`。
- Unix 时间戳（秒），例如 `1713232800`。

示例：

```http
GET /ami/v1/sipflow/flow/abc123?start=2026-04-16T10:00:00%2B08:00&end=2026-04-16T10:30:00%2B08:00
GET /ami/v1/sipflow/media/abc123?start=1713232800&end=1713234600
```

---

## 🛠️ 集成工作流

### 场景 A：CRM 点击拨号

1. 用户在 CRM 中点击电话号码。
2. CRM 后端发送 `POST /api/v1/commands`（未来功能），或使用 AMI 发起呼叫。
3. *当前替代方案*：CRM 向 RustPBX 发送 SIP REFER，或使用一个由 Web 应用注册的专用“点击拨号”SIP 分机。

### 场景 B：AI 语音助手

1. 来电到达 RustPBX。
2. **HTTP Router** 向 AI 后端发送 INVITE 详情。
3. AI 后端返回 `{"action": "forward", "targets": ["sip:ai-bot-service@internal"]}`。
4. RustPBX 通过 SIP/RTP 将音频路由到 AI 机器人。

### 场景 C：计费系统

1. **用户后端** 认证用户，并检查余额大于 0。
2. 通话继续。
3. 挂断时，**CDR 推送** 通过 HTTP POST 将记录发送给计费系统。
4. 计费系统按“时长 × 费率”计算费用并扣减余额。

### 场景 D：合规录音

`[recording].enabled` 启用媒体采集。`[recording].type` 选择媒体去向：`local` / `http` / `s3` 写入 WAV（由 `[recording]` 上传）；`sipflow` 将 RTP 写入 `[sipflow]` 后端（由 `[sipflow.upload].media` 上传）。只要配置了 `[sipflow]` 就会采集 SIP 信令。`auto_start_at = "media"`（默认值）在第一次完成主叫媒体设置后安装录音器；使用 `"answer"` 则等待最终的 200 响应。`discard_unanswered = true` 会删除未接通通话（仅有早期媒体）的 WAV 录音，不保留也不上传；不影响 SipFlow 采集。

#### 方案 1：完整 SipFlow（RTP + SIP）

```toml
[recording]
enabled = true
type = "sipflow"
auto_start = true
auto_start_at = "media"

[sipflow]
type = "local"
root = "./config/sipflow"

[sipflow.upload]
type = "s3"
vendor = "aliyun"
bucket = "my-bucket"
region = "oss-cn-hangzhou"
endpoint = "https://oss-cn-hangzhou.aliyuncs.com"
root = "recordings"
media = true
signaling = true
```

通过 `GET /sipflow/media/{call_id}` 从存储的 RTP 按需生成 WAV，并由 `[sipflow.upload]` 上传。信令以 JSONL 格式上传到同一目标。

#### 方案 2：WAV 文件 + SipFlow 信令

```toml
[recording]
enabled = true
type = "local"   # or "http" / "s3"
auto_start = true
auto_start_at = "media"
path = "./config/recorders"

[sipflow]
type = "local"
root = "./config/sipflow"

[sipflow.upload]
signaling = true
media = false
```

媒体保留在 `[recording]` 路径；SipFlow 仅存储 SIP（不存储 RTP）。

#### 方案 3：仅 WAV（无 SIP 梯形图）

```toml
[recording]
enabled = true
type = "local"
auto_start = true
# No [sipflow] section — WAV only, no signalling capture
```

## 外呼（SSE）

`POST {ami_path}/outbound/dial` 发起呼叫，并通过一个 SSE 连接流式输出该呼叫的每个 RWI 事件。完整请求/响应约定和 `[outbound]` 配置见 [外呼 SSE API](outbound_dial_api.md)。

## 实时语音质检：部署、服务协议与结果查询

本节描述外部质检接口 v1。PBX 负责 SIP、音频解码、调用服务和执行动作；服务负责识别。该接口不绑定 ASR 厂商。每条连接只承载一个方向的单声道音频。

### 1. 在 RustPBX 配置服务

控制台路径：**设置 → 代理配置 → Realtime quality inspection / 实时语音质检**。
填写服务地址、音频方向、接收超时、请求头和参数，点击 **Save proxy settings / 保存代理配置**。请求头输入 JSON 二元数组，参数输入 JSON 对象。保存会写入当前启动配置文件并热更新 PBX，影响后续通话；已经建立的质检连接不切换配置。

对应 TOML：

```toml
[proxy.quality]
enabled = true
url = "ws://127.0.0.1:8095/v1/quality"
sides = "callee"                # caller | callee | both；默认 both
receive_timeout_secs = 15        # 默认 15，必须大于 0
headers = []                    # 可选 [["Authorization", "Bearer your-token"]]

[proxy.quality.parameters]
asr_enabled = true
beep_enabled = true
asr_languages = ["en", "id"]
text_language = "en"
hangup_on_asr = true
hangup_on_beep = true
frequencies_hz = [1309.0, 660.0]
inspect_ms = 15000
```

`enabled` 表示接听后自动开始质检，没有独立的 `auto_start`。`both` 为双方分别建立 WebSocket，不混音。只检测被叫的语音信箱时选择 `callee`。主叫／被叫是 SIP 通话方向，不是 agent/customer 身份。PBX 需代理媒体才能获取音频；wholesale 启用质检的路由会强制媒体通过 PBX。录音仍由录音策略独立控制。

`headers` 用于 WebSocket HTTP Upgrade，例如 Bearer 鉴权；`parameters` 放在首条 `start` 消息中，PBX 原样交给服务。上面的 ASR、beep、hangup 参数属于示例服务，不是 PBX 固定字段。PBX 收到有效 `hangup` 就执行挂断，不需要另一个 `allow_hangup` 开关。

如果服务部署在另一台机器或容器，URL 必须使用 **PBX 所在环境可以访问的地址**；`127.0.0.1` 只指 PBX 自己所在的网络命名空间。局域网可以用 `ws://`；需要传输加密时使用 `wss://`。

也可以通过已有的管理 API 保存：

```bash
curl -X PATCH http://127.0.0.1:8080/api/settings/config/proxy \
  -H 'Authorization: Bearer YOUR_CONSOLE_API_TOKEN' \
  -H 'Content-Type: application/json' \
  --data '{"quality":{"enabled":true,"url":"ws://127.0.0.1:8095/v1/quality","sides":"callee","receive_timeout_secs":15,"headers":[],"parameters":{"beep_enabled":true}}}'
```

`/api` 是默认 API 前缀。调用需要 `system:write` 权限。成功响应为 `status: "ok"`；`requires_restart: false` 表示热更新成功。保存后重新打开设置页，并核对当前 `--conf` 文件中的 `[proxy.quality]`，不要检查另一个工作目录下的同名文件。配置存储在 TOML，不在 `rustpbx_tenants` 或 CDR 数据库里。

### 2. Wholesale 按租户与路由项配置

下例中的 `3` 是租户 ID，`6`、`7` 是路由项 ID，必须换成部署中的真实 ID：

```toml
# PBX 默认不检查；只有显式启用的租户／路由项使用质检。
[proxy.quality]
enabled = false
url = "ws://quality.internal:8095/v1/quality"
sides = "callee"
receive_timeout_secs = 15

[proxy.quality.parameters]
beep_enabled = true

[addons.wholesale.quality.tenants."3"]
enabled = true
url = "ws://tenant-quality.internal:8095/v1/quality"
sides = "callee"
headers = []
receive_timeout_secs = 15

[addons.wholesale.quality.tenants."3".parameters]
asr_enabled = true
asr_languages = ["en", "id"]
hangup_on_asr = true
hangup_on_beep = true

[addons.wholesale.quality.route_items]
"6" = true
"7" = false
```

合并规则：

- 未配置的字段继承 PBX `[proxy.quality]`。
- 租户显式配置覆盖 PBX 默认；`headers` 整组替换，`parameters` 按顶层 key 覆盖，不递归合并内部对象。
- 路由项只覆盖 `enabled`，优先级高于租户。显式 `false` 同样覆盖。
- 没有 wholesale defaults 或 routing profile 层。
- 多目标重试使用实际选中路由项的策略；为了可能启用质检的后续目标，媒体可提前保持代理模式。

**租户／路由项质检策略目前通过 TOML 配置，没有租户表单上的独立质检开关。** 修改 `[addons.wholesale.quality]` 后需重启 wholesale 进程；保存 PBX 代理配置不会重新初始化 addon 策略。

### 3. 服务端 WebSocket 协议

PBX 建立连接（连接超时 3 秒），首先发送 JSON 文本：

```json
{
  "type": "start",
  "version": 1,
  "sample_rate": 8000,
  "call_id": "call-example",
  "session_id": "session-example",
  "side": "callee",
  "direction": "inbound",
  "tenant_id": 3,
  "inbound_trunk_id": 16,
  "outbound_trunk_id": 15,
  "parameters": {"asr_enabled": true, "beep_enabled": true}
}
```

`version` 是接口版本，不是 RustPBX 软件版本。路由 ID 字段在不可用时省略。服务不要依赖 `call_id == session_id`。随后每个 binary 消息都是 **单声道 PCM16 little-endian**，没有 WAV 头，没有 base64，也没有 RTP 头。采样率取自当前方向解码器：PCMU/PCMA 通常 8000，G.722 解码后 16000，Opus 通常 48000。PBX 不统一重采样；每条连接采样率固定，服务按 `sample_rate` 转换到模型需要的采样率。不要把 WebSocket 消息边界当成句子边界。

服务可以不返回识别结果。以下各类结果均独立，可返回零条或多条：

**文字：**

```json
{"type":"text","text":"Please record your message after the tone","partial":false,"start_ms":0,"end_ms":3100,"lang":"en"}
```

`start_ms/end_ms` 为当前音频流内的毫秒偏移，`end_ms >= start_ms`。PBX 为结果补充连接对应的 side。partial 通过 `transcript_segment` 发布，final 只通过 `transcript_final` 发布一次；CC 的 SSE 消费者将两者统一展示。文字事件本身不会自动成为 CDR 的持久化转录记录。

**标记检测结果（不挂断）：**

```json
{
  "type":"calltrace",
  "kind":"quality",
  "code":"quality.voicemail_detected",
  "severity":"info",
  "message":"Voicemail detected",
  "detail":{"method":"asr","language":"en","phrase":"please record your message after the tone"}
}
```

`calltrace` 复用现有 TraceEvent 结构，至少提供 `kind` 和 `message`；`code`、`severity`、`detail` 为附加信息。PBX 生成实际 `ts` 并填写 side，服务不需要返回 side 或时间戳。beep 采用同一个 `code`，并使用 `detail.method = "beep"`；具体频率、持续时间等可放在 detail 中。

**请求挂断：**

```json
{"type":"hangup","reason":"quality.voicemail_detected","detail":{"method":"beep","frequency_hz":1309}}
```

检测到语音信箱并决定挂断时，服务先发 `calltrace`，再发 `hangup`。前者记录观察，后者是动作。只发 `calltrace` 不会挂断。PBX 使用有界命令队列的 `try_send`；队列满时记录日志，不阻塞通话任务。协议没有挂断确认消息，终态以 CDR/通话事件为准。

`{"type":"error","message":"recognizer unavailable"}` 会结束该质检连接并记录日志，电话继续运行。

### 4. 连接寿命与服务实现

服务每 **10 秒发送 WebSocket Ping 控制帧**。PBX 自动回复 Pong；默认 15 秒没有收到任何 WebSocket 消息就关闭质检连接，电话继续运行。不是发送 JSON `{"type":"ping"}`。PBX 不额外主动发 Ping；向服务持续发送音频不能代替服务向 PBX 的心跳。

双方都可以正常发送 Close。服务主动关闭、网络断开、识别窗口结束或服务超时都只结束质检，**不代表挂断电话**。需要挂断必须发 `hangup`。PBX 通话结束时停止业务消息处理，并在独立的最多 1 秒窗口完成 Close 清理；服务不要假设关闭时一定有“最终判断”。

一个仅验证协议和心跳的服务骨架（Python，依赖 `websockets` 的 asyncio API；不执行 ASR，不会自动标记／挂断）：

```python
import asyncio
import json
from websockets.asyncio.server import serve
from websockets.exceptions import ConnectionClosed

async def inspect(ws):
    try:
        first = await asyncio.wait_for(ws.recv(), timeout=5)
        start = json.loads(first)
        if start.get("type") != "start" or start.get("version") != 1:
            await ws.close(code=1008, reason="unsupported start")
            return
        rate = start["sample_rate"]
        print(start["call_id"], start["side"], rate)
        async for message in ws:
            if not isinstance(message, bytes):
                continue
            # message: PCM16LE. Feed your per-connection detector here.
            # Offload CPU inference to a bounded worker queue.
            # No observation means no result message.
    except (ConnectionClosed, asyncio.TimeoutError):
        pass

async def main():
    async with serve(inspect, "127.0.0.1", 8095,
                     ping_interval=10, ping_timeout=10,
                     max_size=1024 * 1024, max_queue=8):
        await asyncio.Future()

asyncio.run(main())
```

识别器产生真实观察后，在连接任务发送结果，例如：

```python
await ws.send(json.dumps({
    "type": "calltrace", "kind": "quality",
    "code": "quality.voicemail_detected", "severity": "info",
    "message": "Voicemail detected",
    "detail": {"method": "asr", "language": "id", "phrase": "layanan pesan suara"}
}))
# Only when your policy requires terminating the phone call:
await ws.send(json.dumps({
    "type": "hangup", "reason": "quality.voicemail_detected",
    "detail": {"method": "asr", "language": "id"}
}))
```

Rust 服务可直接使用 serde 的 tagged enum（`#[serde(tag = "type", rename_all = "lowercase")]`）表示上述消息，再序列化为 WebSocket Text。模型在进程启动时加载共享；每连接单独保留识别流、重采样器、关键词状态和 beep 状态。示例 `realtime-quality` 服务采用每语言一个 sherpa-onnx recognizer/worker、连接独立 stream、有界音频队列、Rubato 重采样到 16 kHz、100 ms ASR 输入块及轻量 beep 检测。不要在 WebSocket 事件循环中同步运行重型推理，以免心跳和关闭处理被阻塞。

### 5. 在控制台查看结果

1. 在 Wholesale CDR 列表按租户、号码或 Call-ID 找到通话，核对 duration、price、cost、profit。
2. 打开关联的核心通话详情 `/console/call-records/{id}`，在 trace 时间线查看 **Quality** 事件及展开详情：`quality.voicemail_detected`、`method`、`language` 和 `side`。
3. 查看最终 **End** 事件，确认 `detail.source = "realtime_quality"`、`detail.reason = "quality.voicemail_detected"`。检测结果和“谁要求挂断”分别记录。
4. 使用详情页的录音和 SIPFlow，检查音频、INVITE/ACK/BYE/200。录音受独立录音策略控制，质检不会自动开启录音。

当前没有 voicemail 顶层列或自动 `tags` 标记，也没有专门的语音信箱列表筛选器。正常服务挂断通常保留顶层 `hangup_reason = "autohangup"`；如还有媒体异常，终态消息可能显示媒体问题，来源仍应查 End 的 detail。

### 6. 从数据库与 API 查询

核心表为 `rustpbx_call_records`，`metadata.trace` 保存检测事件和挂断来源；`wholesale_cdrs` 保存金额，通过 `call_id` 关联。SQL 列名是 snake_case，下载 CDR JSON 的顶层字段是 camelCase。

MySQL 8：

```sql
SELECT c.id, c.call_id, c.hangup_reason, c.recording_url,
       q.method, q.language, q.side,
       w.tenant_id, w.duration, w.price_total, w.cost_total, w.profit
FROM rustpbx_call_records c
JOIN JSON_TABLE(c.metadata, '$.trace[*]' COLUMNS (
  code VARCHAR(100) PATH '$.code',
  method VARCHAR(20) PATH '$.detail.method',
  language VARCHAR(20) PATH '$.detail.language',
  side VARCHAR(20) PATH '$.side'
)) q ON q.code = 'quality.voicemail_detected'
LEFT JOIN wholesale_cdrs w ON w.call_id = c.call_id
ORDER BY c.id DESC;

SELECT c.call_id, e.reason, e.side
FROM rustpbx_call_records c
JOIN JSON_TABLE(c.metadata, '$.trace[*]' COLUMNS (
  kind VARCHAR(30) PATH '$.kind',
  source VARCHAR(50) PATH '$.detail.source',
  reason VARCHAR(100) PATH '$.detail.reason',
  side VARCHAR(20) PATH '$.detail.side'
)) e ON e.kind = 'end' AND e.source = 'realtime_quality'
ORDER BY c.id DESC;
```

SQLite 的对应查询：

```sql
SELECT c.call_id, json_extract(e.value, '$.detail.method') AS method
FROM rustpbx_call_records c, json_each(c.metadata, '$.trace') e
WHERE json_extract(e.value, '$.code') = 'quality.voicemail_detected';
```

PostgreSQL 可使用 `jsonb_array_elements(c.metadata::jsonb->'trace')`，按 `e->>'code'` 过滤。

管理 API：

```text
GET /api/call-records/{id}/cdr-json
GET /api/call-records/{id}/sip-flow?detail=true
GET /api/call-records/{id}/recording
```

这里 `{id}` 是核心表的数值主键，不是 SIP Call-ID；接口沿用通话记录访问权限。

质检挂断不豁免正常计费。已接通的语音信箱仍按租户／供应商各自的费率、最低时长、计费步长计算。测试费率售价 0.10/分钟、成本 0.05/分钟、最低时长和步长均为 60 秒时，即使检测后几秒就挂断，每通仍计售价 0.10、成本 0.05。

### 7. 验证范围（2026-11-11）

完成了真实本地 SIP/PCMU/RTP → PBX → sherpa-onnx/beep 服务 → trace/hangup → 双方 BYE 的端到端测试。英语、印尼语和 1309 Hz beep 均能标记并挂断；核心测试核对连接清理、录音内容、SIPFlow 和唯一终态 CDR。wholesale 回归 113 项通过，另验证了实际通话的数据库 CDR、录音归档、计费及余额扣减。输入为合成语音／音调，不代表运营商实际样本的准确率评估，也未进行压测。

配置保存、热更新和非法输入保护有自动化测试。控制台表单与时间线入口已提供，但本轮浏览器访问受站点权限阻止，**未完成登录后的视觉验证或截图**；不能把 API 验证视为浏览器操作验证。
