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
  "timestamp": 1708201234,
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
| `timestamp` | RustPBX 构造 Webhook 载荷时的 UNIX 时间戳，单位为秒 |
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
  "timestamp": 1708201294,
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
- 通话结束后写入 CDR `metadata["user_data"]`。

`PUT {api_prefix}/calls/active/{session_id}/userdata`

```json
{ "crm_id": "C-1001", "customer": { "tier": "gold" } }
```

响应：`200` 返回 `{ "message": "User data updated", "data": { ... } }`；通话不存在/已结束返回 `404`；body 不是 JSON 对象返回 `400`；超过 16 KiB 返回 `413`。

`GET {api_prefix}/calls/active/{session_id}/userdata` — 回读当前对象（未设置时为空对象）。

### 2.1.1 实时通话转写（SSE）

流式输出活动通话的实时转写文本。第一个订阅者连接时才启动转写，最后一个订阅者断开或通话结束时停止。需要配置 `[proxy.transcript.remote]`（流式 ASR，兼容 Deepgram）；未配置时返回 `503`。

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

`[recording].enabled` 启用媒体采集。`[recording].type` 选择媒体去向：`local` / `http` / `s3` 写入 WAV（由 `[recording]` 上传）；`sipflow` 将 RTP 写入 `[sipflow]` 后端（由 `[sipflow.upload].media` 上传）。只要配置了 `[sipflow]` 就会采集 SIP 信令。`auto_start_at = "media"`（默认值）在第一次完成主叫媒体设置后安装录音器；使用 `"answer"` 则等待最终的 200 响应。

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
