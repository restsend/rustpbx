# RWI Events 开发者参考

> 代码来源：`src/rwi/event.rs`（事件结构）与 `src/rwi/proto.rs`（EventCallContext / RecordingMetadata）｜ 协议版本：`1.0`

---

## 1. 概述

RustPBX 通过 RWI（Real-time WebSocket Interface）实时推送呼叫、IVR、录音、队列、坐席、分机等事件。开发者可以通过以下两种方式接收事件：

| 接收方式 | 协议 | 适用场景 |
|----------|------|----------|
| **WebSocket 订阅** | `ws(s)://<host>/rwi/v1` | 实时双向交互（机器人、软电话、监控面板） |
| **Webhook 回调** | HTTP POST | 异步通知（CRM、录音系统、数据分析平台） |

### 事件分发模型

| 分发方式 | 接收者 | 含义 |
|----------|--------|------|
| `call_owner` | 拥有该 call_id 的 WS 会话 | 单呼叫精细控制 |
| `fan_out` | 订阅了对应 context 的所有 WS 会话 | 来电通知、IVR 事件 |
| `broadcast` | 所有在线 WS 会话 | 全局事件（坐席状态、分机注册等） |
| `webhook` | 配置的 HTTP 端点 | 所有事件均转发（可配置过滤） |

---

## 2. 连接与认证

### WebSocket

```
GET /rwi/v1 HTTP/1.1
Upgrade: websocket
Authorization: Bearer <token>
```

或 URL 参数：`GET /rwi/v1?token=<token>`

### Webhook 配置（rustpbx.toml）

```toml
[rwi_webhook]
url = "https://myapp.example.com/rwi-events"
timeout_ms = 5000
headers = { Authorization = "Bearer your-token" }
# 空 = 全部事件(推荐)。如需白名单过滤,请使用有效的事件类型。
# 注意:坐席状态是 "agent_state_changed"(旧的 "dn_state_changed" 已废弃移除);
# 录音数据(下载 URL、文件大小)通过 "recording_metadata_available" 和
# "record_end" 投递 —— 仅 "record_stopped" 不带录音 URL。
# 白名单示例:
# events = ["call_hangup", "record_stopped", "recording_metadata_available", "record_end", "agent_state_changed"]
events = []
```

---

## 3. 信封格式

### WebSocket 事件

事件字段直接扁平化到 JSON 顶层，并携带一个内嵌的 `event_type` 键标识事件类型：

```json
{
  "event_type": "call_ringing",
  "call_id": "call-abc123",
  "caller_name": "330909",
  "callee_name": "9242000001",
  "direction": "inbound"
}
```

> 不存在 `"rwi"` 或事件名包裹对象；客户端按 `event_type` 字段分发。

### Webhook 信封

```json
{
  "rwi": "1.0",
  "timestamp": 1716212345,
  "call_id": "call-abc123",
  "event_type": "call_ringing",
  "event": {
    /* 与 WS 事件内容完全一致（无 event_type 包裹） */
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `rwi` | string | 协议版本 `"1.0"` |
| `timestamp` | u64 | Unix 时间戳（秒） |
| `call_id` | string | 呼叫标识（广播事件为空字符串） |
| `event_type` | string | snake_case 事件类型名 |
| `event` | object | 事件载荷，字段直接扁平化（无 event_type 包裹） |

---

## 4. 扁平化上下文（EventCallContext）

所有 call-scoped 事件通过 `#[serde(flatten)]` 将以下字段**直接扁平化到事件 JSON 中**（不产生嵌套对象）。`None` 值自动省略不出现在 JSON 里。

| 字段 | 类型 | 说明 |
|------|------|------|
| `caller` | Option\<String\> | 主叫 SIP URI |
| `callee` | Option\<String\> | 被叫 SIP URI |
| `caller_name` | Option\<String\> | 主叫号码（标准化纯号码） |
| `callee_name` | Option\<String\> | 被叫号码 / DNIS |
| `direction` | Option\<String\> | `inbound` / `outbound` / `internal` |
| `trunk` | Option\<String\> | SIP 中继名称 |
| `app_id` | Option\<String\> | IVR 应用 ID |
| `routing_target` | Option\<String\> | 当前路由目标 |
| `root` | Option\<Object\> | 根呼叫标识（嵌套对象，见下） |

**`root`（根呼叫）** — 标识当前呼叫的根（跨转接保持不变）：

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 根呼叫 ID |
| `caller` / `callee` | Option\<String\> | 根呼叫的主 / 被叫 |

**说明**：
- `ani` vs `caller`：`ani` 是纯号码（用于业务匹配），`caller` 是完整 SIP URI
- `dnis` vs `callee`：同上
- 上下文由 `CallMetaStore` 在 gateway 分发时自动注入，事件生产者无需手动填充

### 来源字段（gateway 自动注入）

集群模式下 gateway 还会给**所有**事件注入以下字段，Webhook 与 WS 消费者都可见：

| 字段 | 类型 | 说明 |
|------|------|------|
| `src_ip` | Option\<String\> | 本节点 cluster 内 IP（来自 `[cluster].peers` 自匹配）。仅在启用 cluster 且匹配到本机 peer 时注入；单机/NAT 不匹配时不出现 |
| `client_ip` | Option\<String\> | 坐席客户端注册 IP（`Location.destination`，WS/WebRTC 即 cc-phone 源 IP）。仅当事件载荷带 `agent_id` 且该坐席已注册时注入 |

两者都遵循“事件自身字段优先”约定——事件自身携带同名非空字段时不会被覆盖。`client_ip` 在坐席注册/注销时从 locator 捕获，分发时 O(1) 查 registry，无发射时查询。

### 字段重复说明

部分事件（如 `RecordStopped`、`IvrNodeEntered`）自身也携带 `ani`/`dnis` 等字段。当事件自身字段值为 `None` 时，`enrich()` 会自动从上下文补充。Webhook 消费者最终收到的是合并后的完整值。

---

## 5. 订阅与断线重连

### 订阅 context

```json
{
  "rwi": "1.0",
  "action_id": "sub-001",
  "action": "session.subscribe",
  "params": { "contexts": ["queue:support", "agent:*"] }
}
```

| Context 格式 | 说明 |
|---------------|------|
| `queue:<queue_id>` | 订阅指定队列事件 |
| `agent:<agent_id>` | 订阅指定坐席事件 |
| `*` | 通配，接收所有广播事件 |

### 断线重连（Session Resume）

```json
{
  "rwi": "1.0",
  "action_id": "resume-001",
  "action": "session.resume",
  "params": {}
}
```

服务端缓存最近 1000 条事件（保留 60 秒），重连后自动回放全部缓存事件（`call.resume` 携带 `call_id` 时只回放该呼叫的事件）。客户端可按 `call_id` + `event_type` 自行幂等去重。

### Webhook 去重

Webhook 使用 `(call_id, timestamp)` 元组去重，环形缓冲区容量 4096 条。重复事件自动丢弃。

---

## 6. 完整事件字典

> 下方各表 `+ctx` 表示该事件携带扁平化上下文字段。
> `?` 表示 `Option<T>` 字段，值为 `null` 时省略。

### 6.1 呼叫生命周期

#### call_created

分发：call_owner

呼叫创建并进入拨号（calling）阶段——入呼 INVITE 与 API 外呼（`call.originate` / outbound dial）都会触发。是任何呼叫流程的第一个事件。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫唯一标识 |
| `context` | String | 拨号计划 context |
| `caller` | String | 主叫 SIP URI |
| `callee` | String | 被叫 SIP URI |
| `trunk` | Option\<String\> | SIP 中继名 |
| `sip_headers` | Map\<String, String\> | 白名单 SIP 头 |
| `caller_name` | Option\<String\> | 主叫号码 |
| `callee_name` | Option\<String\> | 被叫号码 / DNIS |
| `called_phone` | Option\<String\> | 实际被叫号码（外呼场景） |
| `app_id` | Option\<String\> | IVR 应用 ID |
| `routing_target` | Option\<String\> | 路由目标 |
| `uuid` | Option\<String\> | 全局 UUID（关联录音） |
| `routing_path` | Option\<Vec\<String\>\> | 路由路径 |
| `session_id` | Option\<String\> | enrichment：逻辑呼叫根 session_id |
| `direction` | Option\<String\> | enrichment：`inbound` / `outbound` / `internal` |

> **注意**：所有 call 事件统一使用上下文注入的 `direction` 字段（由 `CallMetaStore` enrichment 注入）。跨腿关联请用 enrichment 注入的 `session_id`。

```json
{
  "rwi": "1.0",
  "call_created": {
    "call_id": "call-abc",
    "context": "inbound",
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "direction": "inbound",
    "trunk": "trunk_sip",
    "sip_headers": { "X-Tenant": "corp_a" },
    "session_id": "call-abc",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "called_phone": null,
    "app_id": "ivr_sales",
    "routing_target": "queue:support",
    "uuid": "uuid-abc-123",
    "routing_path": ["menu:root", "queue:level1"]
  }
}
```

#### call_ringing / call_early_media / call_answered / call_unbridged / call_no_answer / call_busy

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| *+ctx* | | 扁平化上下文 |

```json
{
  "rwi": "1.0",
  "call_ringing": {
    "call_id": "call-abc",
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "direction": "inbound"
  }
}
```

#### call_bridged

分发：call_owner（两条 leg 均收到）

| 字段 | 类型 | 说明 |
|------|------|------|
| `leg_a` | String | A 腿 call_id |
| `leg_b` | String | B 腿 call_id |

#### call_hangup

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `reason` | Option\<String\> | 挂机原因（见下表） |
| `hangup_by` | Option\<String\> | 归一化发起方：`agent` / `caller` / `system` / `transfer` / `unknown`（与 `cc_hangup.hangup_by` 同词汇表；未涉及坐席的被叫挂机记为 `callee`） |
| `sip_status` | Option\<u16\> | SIP 响应码 |
| *+ctx* | | 扁平化上下文 |

**reason 枚举值**：

| 值 | 说明 |
|----|------|
| `caller` | 主叫挂机 |
| `callee` | 被叫挂机 |
| `refer` | REFER 转接挂机 |
| `system` | 系统挂机 |
| `autohangup` | 自动挂机（超时） |
| `noAnswer` | 无应答（408/480/487） |
| `rejected` | 拒接/忙（486/600/603） |
| `canceled` | 取消（487） |
| `failed` | 通用失败（其他 4xx） |
| `serverUnavailable` | 服务不可用（5xx） |
| `rtpTimeout` | RTP 超时 |

```json
{
  "rwi": "1.0",
  "call_hangup": {
    "call_id": "call-abc",
    "reason": "caller",
    "sip_status": null,
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "direction": "inbound"
  }
}
```

### 6.2 转接事件

#### call_transferred / call_transfer_accepted

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `transfer_target` | Option\<String\> | 原始转接目标字符串（如 `queue:queue-name?target=skillgroup:tech-support_G`）。在 SIP REFER Replaces 接管等场景下为 `None`。 |
| *+ctx* | | 扁平化上下文 |

#### call_transfer_failed

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `sip_status` | Option\<u16\> | SIP 状态码 |
| `reason` | Option\<String\> | 失败原因 |
| `transfer_target` | Option\<String\> | 原始转接目标字符串（同上） |
| *+ctx* | | 扁平化上下文 |

### 6.3 媒体事件

#### media_hold_started / media_hold_stopped

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| *+ctx* | | 扁平化上下文 |

> `media_stream_started` / `media_stream_stopped` 已随 `media.stream_start`
> / `media.inject_start` 命令一并移除。实时双向 PCM 改用
> `call.transfer` → `voip_bridge:` WebSocket 端点（呼入/外呼均支持），
> 不再产生这两个事件。

#### media_ringback_passthrough_started / media_ringback_passthrough_stopped

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `source` | String | 源 leg call_id |
| `target` | String | 目标 leg call_id |

#### media_play_started / media_play_finished

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `leg_id` | Option\<String\> | 目标 leg |
| `track_id` | String | 播放 track ID |
| `interrupted` | bool | `media_play_finished` 专用：是否被 DTMF 中断 |
| *+ctx* | | 扁平化上下文 |

#### dtmf

分发：fan_out_to_context

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `digit` | String | DTMF 按键（`0`-`9`、`*`、`#`） |
| `leg_id` | Option\<String\> | 产生 DTMF 的 leg |
| `extra` | Option\<Object\> | 附加数据（扩展字段，默认 `null`） |
| *+ctx* | | 扁平化上下文 |

#### dtmf_collected / dtmf_collection_timeout

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `leg_id` | String | DTMF 来源 leg |
| `digits` | String | `dtmf_collected` 专用：收集到的按键串 |
| *+ctx* | | 扁平化上下文 |

---

### 6.4 录音事件

#### record_started / record_paused / record_resumed / record_failed

分发：call_owner

> 触发方式：通过 `RecordStart` / `RecordPause` / `RecordResume` / `RecordStop` RWI 命令触发，**非自动**。录音不会在通话接通后自动开始。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `error` | String | `record_failed` 专用：错误信息 |
| *+ctx* | | 扁平化上下文 |

#### record_stopped（增强版）

分发：call_owner

> 触发方式：通过 `RecordStop` RWI 命令触发，**非自动**。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `duration_secs` | Option\<u64\> | 录音时长（秒） |
| `filename` | Option\<String\> | 录音文件名 |
| `unique_id` | Option\<String\> | 录音 UUID |
| `file_size` | Option\<u64\> | 文件大小（字节） |
| `download_url` | Option\<String\> | 下载地址 |
| `caller_name` | Option\<String\> | 主叫号码 |
| `callee_name` | Option\<String\> | 被叫号码 |
| `called_phone` | Option\<String\> | 实际被叫号码 |
| `call_type` | Option\<String\> | `inbound`/`outbound`/`internal`/`consult` |
| `agent_id` | Option\<String\> | 坐席 ID |
| `agent_name` | Option\<String\> | 坐席名称 |
| `call_start_time` | Option\<String\> | 通话开始时间（ISO 8601） |
| `call_end_time` | Option\<String\> | 通话结束时间 |
| `upload_time` | Option\<String\> | 上传完成时间 |
| `switch_flag` | Option\<String\> | 站点标识（如 `ks`、`bj`） |
| `session_id` | Option\<String\> | enrichment：逻辑呼叫根 session_id |

> 注意：`record_stopped` 不携带完整扁平化上下文的 typed 字段，但 `enrich()` 会从 CallMetaStore 补充 `session_id` 等缺失键。旧字段 `root_call_id` 已移除。

```json
{
  "rwi": "1.0",
  "record_stopped": {
    "call_id": "call-abc",
    "duration_secs": 51,
    "filename": "uuid_2026-05-14_08-11-49.mp3",
    "unique_id": "uuid-abc-123",
    "file_size": 149517,
    "download_url": "https://storage.example.com/rec.mp3",
    "caller_name": "330909",
    "callee_name": "9242000001",
    "called_phone": "018659727661",
    "call_type": "outbound",
    "agent_id": "451447",
    "agent_name": "luoxiaofeng90_v",
    "call_start_time": "2026-05-14T08:11:35Z",
    "call_end_time": "2026-05-14T08:12:26Z",
    "upload_time": "2026-05-14T16:14:46Z",
    "switch_flag": "ks",
    "session_id": "call-root-42"
  }
}
```

#### recording_metadata_available

分发：call_owner

录音文件上传完成后触发，包含完整元数据。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `metadata` | RecordingMetadata | 录音元数据（见下表） |

**RecordingMetadata 字段**（typed 字段 + `extra` 透传袋）：

| 字段 | 类型 | 说明 |
|------|------|------|
| `filename` | String | 录音文件名 |
| `file_size` | u64 | 文件大小（字节） |
| `download_url` | Option\<String\> | 下载地址 |
| `caller_name` / `callee_name` | Option\<String\> | 主 / 被叫号码 |
| `call_type` | String | 呼叫类型 |
| `call_start_time` / `call_end_time` / `upload_time` | Option\<String\> | 通话开始 / 结束 / 上传完成时间 |
| *(其他任意键)* | String | `extra` 透传袋（`#[serde(flatten)]`）：addon 写入的扁平字符串键（如 `agent_id`、`queue_id`、`tenant_id`、`switch_flag`）原样透传，核心不命名 |

> 注意：不存在 `unique_id` typed 字段；`agent_id` 等业务字段依赖 addon 是否写入 `extra`。

```json
{
  "rwi": "1.0",
  "recording_metadata_available": {
    "call_id": "call-abc",
    "metadata": {
      "filename": "uuid_2026-05-14.mp3",
      "unique_id": "uuid-abc-123",
      "file_size": 149517,
      "download_url": "https://storage.example.com/rec.mp3",
      "caller_name": "330909",
      "callee_name": "9242000001",
      "called_phone": null,
      "call_type": "inbound",
      "agent_id": "451447",
      "agent_name": "luoxiaofeng90_v",
      "call_start_time": "2026-05-14T08:11:35Z",
      "call_end_time": "2026-05-14T08:12:26Z",
      "upload_time": "2026-05-14T16:14:46Z",
      "switch_flag": "ks",
      "process_flag": "ks_22_normal",
      "session_id": "call-root-42"
    }
  }
}
```

#### record_end

分发：call_owner

录音终结事件。在录音上传完成后触发；若无上传配置则在录音文件就绪后触发（使用本地文件路径）。SipFlow 媒体上传完成后也会触发。

> **触发条件**：
> - 普通录音：`CallRecordManager` 处理完录音记录后，`RecordingUploadHook` 自动触发
> - SipFlow 录音：SipFlow 媒体文件上传到 S3/HTTP 完成后自动触发
> - **不**需要通过 `RecordStop` 命令触发，与 `record_started`/`record_stopped` 由 command 触发的模式不同

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `url` | Option\<String\> | 上传 URL（有上传时）或本地文件路径（无上传时），SipFlow 场景为媒体文件 URL |
| `duration_secs` | u64 | 录音时长（秒） |
| `file_size` | u64 | 文件大小（字节） |

#### transcript_started / transcript_segment / transcript_error / transcript_ended

分发：call_owner

> 触发方式：懒启动——首个 SSE 订阅者连接 `GET /cc/calls/{call_id}/transcript`（或发送 `StartTranscription` 命令）时自动开始；最后一个订阅者断开或通话挂断时停止。参见 [Live Transcript SSE API](live_transcript_api.md)。

**transcript_started**

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `sides` | Vec\<String\> | 开启 ASR 流的侧（`"caller"` / `"callee"`） |
| `provider` | Option\<String\> | provider 标识（如 `"deepgram"`） |

**transcript_segment**

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `side` | String | `"caller"` / `"callee"` |
| `text` | String | 识别文本 |
| `partial` | bool | `true` = 中间假设（会被同侧后续 segment 替换），`false` = 最终结果 |
| `start_ms` | u64 | 相对转录起点偏移（毫秒） |
| `end_ms` | u64 | 相对转录起点偏移（毫秒） |
| `lang` | Option\<String\> | 语言 |

**transcript_error**

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `side` | Option\<String\> | `null` = 整个 provider 不可用（流终止）；非 null = 单侧失败（流继续） |
| `error` | String | 错误信息 |

**transcript_ended**

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `reason` | String | `"stopped"`（订阅者全部断开）/ `"call_ended"` 等 |

---

### 6.5 IVR 事件

所有 IVR 事件携带扁平化上下文。

#### ivr_node_entered

分发：fan_out_to_context

呼叫进入 IVR 节点（菜单、播放提示音等）。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `node_id` | String | 节点 ID |
| `node_name` | String | 节点名称 |
| `node_type` | String | 节点类型（`menu`、`prompt`、`transfer` 等） |
| `app_id` | String | IVR 应用 ID |
| `entry_time` | String | 进入时间（ISO 8601） |
| `caller_name` | Option\<String\> | 主叫号码 |
| `callee_name` | Option\<String\> | 被叫号码 |
| `routing_target` | Option\<String\> | 路由目标 |
| `previous_node_id` | Option\<String\> | 上一个节点 ID |
| *+ctx* | | 扁平化上下文 |

#### ivr_node_exited

分发：fan_out_to_context

呼叫退出 IVR 节点。

> **会话终止时也会触发**：当 sip_session 在执行中被终止（主叫挂机、系统取消等），内置（tree 模式）IVR 也会 emit 本事件记录主叫当时所在的节点，此时 `hangup_reason` 被填充（取值：`cancelled`、`remote_hangup`、`hangup`、`error` 等），`call_result` 为 `"hangup"`。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `node_id` | String | 节点 ID |
| `node_name` | String | 节点名称 |
| `result_value` | Option\<String\> | 用户按键或分支结果 |
| `duration_ms` | u32 | 节点停留时长（毫秒） |
| `exit_time` | String | 退出时间 |
| `next_node_id` | Option\<String\> | 下一个节点 ID |
| `hangup_reason` | Option\<String\> | 挂机原因（会话终止时取值：`cancelled`/`remote_hangup`/`hangup` 等） |
| `call_result` | Option\<String\> | 通话结果 |
| *+ctx* | | 扁平化上下文 |


#### ivr_flow_completed

分发：fan_out_to_context

IVR 流程完成（执行了终止动作：转接、排队、留言、挂机）。

> **会话终止时也会触发**：内置（tree 模式）IVR 在 sip_session 执行中被终止（主叫挂机 `remote_hangup`、系统取消 `cancelled` 等）时会以 `final_result` 记录终止原因，并携带 `total_nodes_traversed` 统计信息。`final_result` 取值：`transferred`、`queue`、`voicemail`、`hangup`、`abandoned`、`cancelled`、`remote_hangup`、`error` 等。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `app_id` | String | IVR 应用 ID |
| `total_nodes_traversed` | u32 | 经过的节点总数 |
| `total_duration_ms` | u32 | IVR 总耗时（毫秒） |
| `final_result` | String | 最终结果（`transferred`、`voicemail`、`abandoned`、`cancelled`、`remote_hangup` 等） |
| `completion_time` | String | 完成时间 |
| `final_routing_target` | Option\<String\> | 最终路由目标 |
| *+ctx* | | 扁平化上下文 |

```json
{
  "rwi": "1.0",
  "ivr_flow_completed": {
    "call_id": "call-abc",
    "app_id": "ivr-sales",
    "total_nodes_traversed": 3,
    "total_duration_ms": 15200,
    "final_result": "transferred",
    "completion_time": "2026-05-14T17:55:00Z",
    "final_routing_target": "queue:support",
    "caller": "13800138000",
    "direction": "inbound"
  }
}
```

#### ivr_step_trace

分发：fan_out_to_context

Step-Mode IVR 跟踪事件。每一步 provider 往返或动作执行完成时产生。

> **会话终止条目（`session_end`）**：当 IVR 会话结束（含主叫挂机 `RemoteHangup`、系统取消 `Cancelled`）时，会额外 emit 一条 `trigger.type="session_end"` 的跟踪事件，`action_type`/`step_id`/`step_name` 记录最后执行的节点，并填充 `end_reason`/`end_detail` 表示整个会话的结束原因。外部 provider 的 `/end` webhook 在 `RemoteHangup`/`Cancelled` 时不会被调用（本地跟踪事件照常发出）。
>
> **单条完成事件**：每个步骤（含等待类：播放、收号、转接等待结果）只在完成时发出**一条**跟踪事件。`trigger` 保留触发该步骤的原始来源（如 `phone_collected`、`dtmf`）及 detail，以 `step_end_time` 有值作为完成标记；不发送任何中间态或重复事件。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `session_id` | String | 会话 ID |
| `caller` | String | 主叫 |
| `callee` | String | 被叫 |
| `step_index` | u32 | 步骤序号 |
| `trigger` | Object | 触发该步骤的结构化信息，见下方说明 |
| `action_type` | String | 动作类型（如 `Transfer`、`Prompt`、`DtmfMenu`） |
| `action_json` | Option\<String\> | 动作详情 JSON |
| `duration_ms` | u64 | 步骤执行耗时（毫秒），始终有值 |
| `error` | Option\<String\> | 错误信息 |
| `step_id` | Option\<String\> | 当前节点 ID，由 Provider 通过 ActionNode.step_id 返回 |
| `step_name` | Option\<String\> | 当前节点名称，由 Provider 通过 ActionNode.step_name 返回 |
| `step_start_time` | Option\<String\> | 当前步骤开始时间（ISO UTC）。常规步骤有值；`session_end`、fallback、bridge 按键等派生条目为 null |
| `step_end_time` | Option\<String\> | 当前步骤结束时间（ISO UTC），始终有值 —— 它是该步骤完成（事件已发出）的标记 |
| `extra` | Option\<JSON Object\> | Provider 透传的额外数据。Provider 在每次响应的 ActionNode.extra 中返回完整对象，RustPBX 透传存储并原样输出 |
| `sip_headers` | Option\<Map\<String, String\>\> | 呼叫的白名单 SIP 头 |
| `end_reason` | Option\<String\> | 仅会话终止（`session_end`）条目有值，标识整个 IVR 会话如何结束（`normal`、`transfer`、`transfer_to_queue`、`hangup`、`user_hangup`、`timeout`、`error` 等） |
| `end_detail` | Option\<String\> | 与 `end_reason` 配套的详情（如转接目标、错误信息） |

> **`trigger` 字段说明**：
>
> 描述触发当前步骤执行的原因，是一个对象：
>
> ```json
> { "type": "dtmf", "detail": { "digit": "2" } }
> ```
>
> | 子字段 | 类型 | 说明 |
> |--------|------|------|
> | `type` | String | 触发源类型：`session_start`、`session_end`、`dtmf`、`dtmf_menu`、`dtmf_menu_timeout`、`audio_complete`、`action_execute`、`chained`、`api_response`、`phone_collected`、`recording_complete`、`input_voice`、`error`、`dtmf_menu_invalid`、`unknown` |
> | `detail` | Option\<JSON Object\> | 触发详情对象，无详情时省略。常见取值：DTMF → `{"digit":"2"}`；API 响应 → `{"status":200}`；号码收集 → `{"number":"13800138000"}` |
>
> **时间字段说明**：
> - `step_start_time` — 当前步骤的开始时间（上一步结束或 session 开始）
> - `step_end_time` — 步骤结束时间，每条事件都有（完成标记）
>
> **耗时字段说明**：
> - `duration_ms` — 步骤执行耗时（毫秒），始终有值，包含 provider 往返和动作执行时间

---

### 6.6 队列 / ACD 事件

> **事件来源说明**：队列相关事件分两个家族，由不同子系统产生，可同时出现：
> - **`queue_*`（队列生命周期）**：由 Queue 应用（`src/call/app/queue.rs`）产生，**无论是否启用 CC addon 都会发**。覆盖入队、振铃、接通、放弃、超时、回退等通用生命周期。
> - **`skill_group_*`（技能组调度决策）**：由 CC addon 的 ACD 适配器（`src/addons/cc/agent_registry_adapter.rs`）在队列向 ACD 询问坐席、ACD 产出调度结果时产生，**仅在启用 CC addon 且使用技能路由时发**。
>
> 一通走技能组的呼叫，典型事件序列：
> `queue_joined` → `skill_group_candidates_found` → `skill_group_agent_assigned` → `queue_agent_offered` → `queue_agent_connected`

所有队列事件携带扁平化上下文。

> **session_id 关联（2026-08 起）**：所有呼叫域事件（`queue_*` / `skill_group_*` /
> `cc_*` / `call_*`）的 payload 通过 CallMetaStore enrichment 自动携带
> `session_id` 字段 —— 整通逻辑呼叫的根会话 ID（首 INVITE 的 Call-ID，跨转接/
> 派发/consult 不变）。`call_id` 是当前腿的标识，转接产生的新腿会变化。
> 详见 [session_id_correlation.md](./session_id_correlation.md)。

#### queue_joined

分发：call_owner / broadcast

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| *+ctx* | | 扁平化上下文 |

#### queue_position_changed

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `position` | u32 | 当前排队位置 |
| *+ctx* | | 扁平化上下文 |

#### queue_agent_offered / queue_agent_connected

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `agent_id` | String | 坐席 ID |
| *+ctx* | | 扁平化上下文 |

#### queue_left

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `reason` | Option\<String\> | 离开原因 |
| *+ctx* | | 扁平化上下文 |

#### queue_wait_timeout

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| *+ctx* | | 扁平化上下文 |


#### queue_voicemail_redirected

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `reason` | String | 原因 |
| *+ctx* | | 扁平化上下文 |

#### queue_candidates_found

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `candidates` | Vec\<String\> | 候选坐席列表 |
| `trace_id` | String | ACD 跟踪 ID |
| *+ctx* | | 扁平化上下文 |

#### queue_agent_ringing / queue_agent_no_answer / queue_agent_rejected

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `agent_id` | String | 坐席 ID |
| `attempt` | u32 | `no_answer`/`rejected` 专用：尝试次数 |
| `trace_id` | String | ACD 跟踪 ID |
| *+ctx* | | 扁平化上下文 |

#### queue_fallback_executed

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `queue_id` | String | 队列 ID |
| `action` | String | 执行的回退动作 |
| `reason` | String | 原因 |
| `trace_id` | String | ACD 跟踪 ID |
| *+ctx* | | 扁平化上下文 |

#### queue_alert

分发：broadcast（无 call_id）

| 字段 | 类型 | 说明 |
|------|------|------|
| `queue_id` | String | 队列 ID |
| `alert_type` | String | 告警类型 |
| `message` | String | 告警消息 |

#### skill_group_candidates_found

分发：broadcast

ACD 调度器为技能组找到候选坐席时触发。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `skill_group_id` | Option\<String\> | 技能组 ID（显式 `skill-group:{id}` 路径为 `Some`；自主技能路由为 `None`） |
| `candidates` | Vec\<String\> | 候选坐席 ID 列表 |
| `trace_id` | String | 跟踪 ID |

#### skill_group_agent_assigned

分发：broadcast

ACD 调度器决定将某坐席分配给该呼叫时触发（ACD `Assign` 决策或策略选中的首位坐席）。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `skill_group_id` | Option\<String\> | 技能组 ID |
| `agent_id` | String | 被分配的坐席 ID |
| `trace_id` | String | 跟踪 ID |

#### skill_group_no_agent

分发：broadcast

ACD 调度器无法为技能组提供坐席时触发。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `skill_group_id` | Option\<String\> | 技能组 ID |
| `reason` | String | 原因（`no_candidates` 无匹配坐席 / `acd_blocked` 被 ACD 策略拦截 / `no_strategy_match` 策略未选中） |

---

### 6.7 坐席状态事件

#### agent_state_changed

分发：broadcast

坐席状态机转换。

| 字段 | 类型 | 说明 |
|------|------|------|
| `agent_id` | String | 坐席 ID |
| `from_status` | String | 原状态 |
| `to_status` | String | 新状态 |
| `call_id` | Option\<String\> | 关联呼叫 ID |
| `agent_name` | Option\<String\> | 坐席名称 |
| `agent_extension` | Option\<String\> | 坐席分机号 |
| `caller` | Option\<String\> | 主叫 / 分机号 |
| `team_id` | Option\<String\> | 团队 ID |
| `duration_secs` | Option\<u32\> | 上一状态持续时长 |
| `reason_code` | Option\<String\> | 原因码（如 `CALL`、`BREAK`、`TRAINING`） |

**坐席状态枚举**：

| 状态 | 说明 | 可转到 |
|------|------|--------|
| `offline` | 离线 | `idle`、`away`、`dnd` |
| `idle` | 空闲（可接听） | `ringing`、`away`、`dnd`、`offline` |
| `away` | 离开（小休） | `idle`、`dnd`、`offline` |
| `dnd` | 勿扰 | `idle`、`away`、`offline` |
| `ringing` | 振铃中（含 call_id） | `busy`（接听）、`idle`（未接） |
| `busy` | 通话中（含 call_id） | `wrapup` |
| `wrapup` | 话后处理 | `idle`、`away`、`dnd` |
| `custom:<name>` | 自定义状态 | `idle`、`away`、`dnd`、`offline` |

```json
{
  "rwi": "1.0",
  "agent_state_changed": {
    "agent_id": "agent-001",
    "from_status": "idle",
    "to_status": "busy",
    "call_id": "call-abc",
    "agent_name": "Alice",
    "agent_extension": "8001",
    "caller": "8001",
    "team_id": "sales",
    "duration_secs": 300,
    "reason_code": "CALL"
  }
}
```

---

#### cc_ringing / cc_answered / cc_held / cc_unheld

CC 坐席呼叫生命周期事件（addon-cc）。`cc_ringing`：坐席话机振铃；
`cc_answered`：坐席接听；`cc_held` / `cc_unheld`：坐席侧保持 / 恢复。
载荷含 `call_id`、`agent_id`、`queue_id` 与扁平化上下文。

#### cc_hangup

CC 呼叫挂机。载荷含 `call_id`、`agent_id`、`hangup_by`
（`agent` / `caller` / `system` / `transfer` / `unknown`）、`talk_secs`。

#### skill_group_call_queued / skill_group_call_abandoned / skill_group_service_unavailable

技能组排队事件：呼叫进入 / 放弃技能组排队；无可用坐席。
载荷含 `call_id`、`skill_group_id`、`waiting_count` 等。


### 6.10 会议事件

#### conference_created / conference_destroyed

分发：broadcast

| 字段 | 类型 | 说明 |
|------|------|------|
| `conf_id` | String | 会议房间 ID |

#### conference_member_joined / conference_member_left / conference_member_muted / conference_member_unmuted

分发：broadcast

| 字段 | 类型 | 说明 |
|------|------|------|
| `conf_id` | String | 会议 ID |
| `call_id` | String | 成员呼叫 ID |
| *+ctx* | | 扁平化上下文 |

#### conference_ended_by_host

| 字段 | 类型 | 说明 |
|------|------|------|
| `conf_id` | String | 会议 ID |
| `host_call_id` | String | 主持人呼叫 ID |
| `removed_call_ids` | Vec\<String\> | 被移除的成员 |
| *+ctx* | | 扁平化上下文 |

#### conference_auto_ended

| 字段 | 类型 | 说明 |
|------|------|------|
| `conf_id` | String | 会议 ID |
| `reason` | String | 结束原因 |
| *+ctx* | | 扁平化上下文 |

#### conference_error

| 字段 | 类型 | 说明 |
|------|------|------|
| `conf_id` | String | 会议 ID |
| `error` | String | 错误信息 |

#### conference_consult_dialing / conference_consult_connected

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 咨询呼叫 ID |
| `target` | String | 咨询目标 |
| *+ctx* | | 扁平化上下文 |

#### conference_merge_requested / conference_merged / conference_merge_failed

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫 ID（`merge_requested` 含 `consultation_call_id`） |
| `conf_id` | String | 会议 ID（`merged`/`merge_failed`） |
| `consultation_call_id` | String | `merge_requested` 专用：咨询呼叫 ID |
| `reason` | String | `merge_failed` 专用：失败原因 |
| *+ctx* | | 扁平化上下文 |

#### conference_seat_replace_started / ...succeeded / ...failed / ...rollback_failed

| 字段 | 类型 | 说明 |
|------|------|------|
| `conf_id` | String | 会议 ID |
| `old_call_id` | String | 原成员呼叫 ID |
| `new_call_id` | String | 新成员呼叫 ID |
| `reason` | String | `failed`/`rollback_failed` 专用：失败原因 |

**座位替换事件序列（成功路径）**：
1. `conference_seat_replace_started`
2. `conference_member_left`（旧成员离开）
3. `conference_member_joined`（新成员加入）
4. `conference_seat_replace_succeeded`

---

### 6.11 管理监控事件

#### supervisor_listen_started / supervisor_whisper_started / supervisor_barge_started / supervisor_takeover_started

| 字段 | 类型 | 说明 |
|------|------|------|
| `supervisor_call_id` | String | 管理员呼叫 ID |
| `target_call_id` | String | 被监控呼叫 ID |

#### supervisor_mode_stopped

| 字段 | 类型 | 说明 |
|------|------|------|
| `supervisor_call_id` | String | 管理员呼叫 ID |
| `target_call_id` | String | 被监控呼叫 ID |

---

### 6.13 SIP 信令事件

#### sip_message_received / sip_notify_received

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `content_type` | String | 内容类型 |
| `body` | String | 消息内容 |
| `event` | String | `sip_notify_received` 专用：SIP Event 头 |
| *+ctx* | | 扁平化上下文 |

---

### 6.14 会话系统事件

#### call_ownership_changed

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `session_id` | String | 接管会话 ID |
| `mode` | String | 模式（`control`/`listen`/`whisper`/`barge`） |
| *+ctx* | | 扁平化上下文 |

#### session.resume / call.resume（命令结果，非事件）

| 字段 | 类型 | 说明 |
|------|------|------|
| `replayed_count` | u64 | 回放的缓存事件条数 |
| `events` | array | 回放条目（`timestamp` / `call_id` / `event`） |

---

## 7. 事件类型速查表

| 事件类型 | 分发 | call_id | 上下文 |
|----------|------|---------|--------|
| `call_created` | owner | ✅ | 自有字段（入呼 INVITE 与外呼 originate） |
| `call_ringing` | owner | ✅ | +ctx |
| `call_early_media` | owner | ✅ | +ctx |
| `call_answered` | owner | ✅ | +ctx |
| `call_bridged` | owner | leg_a | — |
| `call_unbridged` | owner | ✅ | +ctx |
| `call_transferred` | owner | ✅ | +ctx |
| `call_transfer_accepted` | owner | ✅ | +ctx |
| `call_transfer_failed` | owner | ✅ | +ctx |
| `call_hangup` | owner | ✅ | +ctx |
| `call_no_answer` | owner | ✅ | +ctx |
| `call_busy` | owner | ✅ | +ctx |
| `media_hold_started` | owner | ✅ | +ctx |
| `media_hold_stopped` | owner | ✅ | +ctx |
| `media_ringback_passthrough_started` | owner | ✅ | — |
| `media_play_started` | owner | ✅ | +ctx |
| `media_play_finished` | owner | ✅ | +ctx |
| `record_started` | owner | ✅ | +ctx |
| `record_paused` | owner | ✅ | +ctx |
| `record_resumed` | owner | ✅ | +ctx |
| `record_stopped` | owner | ✅ | 自有字段+enrich |
| `record_failed` | owner | ✅ | +ctx |
| `recording_metadata_available` | owner | ✅ | — |
| `transcript_started` | owner | ✅ | 自有字段 |
| `transcript_segment` | owner | ✅ | 自有字段+enrich |
| `transcript_error` | owner | ✅ | 自有字段+enrich |
| `transcript_ended` | owner | ✅ | 自有字段 |
| `dtmf` | fan_out | ✅ | +ctx |
| `dtmf_collected` | owner | ✅ | +ctx |
| `dtmf_collection_timeout` | owner | ✅ | +ctx |
| `ivr_node_entered` | fan_out | ✅ | +ctx |
| `ivr_node_exited` | fan_out | ✅ | +ctx |
| `ivr_flow_completed` | fan_out | ✅ | +ctx |
| `ivr_step_trace` | fan_out | ✅ | — |
| `queue_joined` | owner/broadcast | ✅ | +ctx |
| `queue_position_changed` | owner | ✅ | +ctx |
| `queue_agent_offered` | broadcast | ✅ | +ctx |
| `queue_agent_connected` | owner | ✅ | +ctx |
| `queue_left` | broadcast | ✅ | +ctx |
| `queue_wait_timeout` | owner | ✅ | +ctx |
| `queue_candidates_found` | owner | ✅ | +ctx |
| `queue_agent_ringing` | owner | ✅ | +ctx |
| `queue_agent_no_answer` | owner | ✅ | +ctx |
| `queue_agent_rejected` | owner | ✅ | +ctx |
| `queue_fallback_executed` | owner | ✅ | +ctx |
| `queue_alert` | broadcast | — | — |
| `skill_group_candidates_found` | broadcast | ✅ | — |
| `skill_group_agent_assigned` | broadcast | ✅ | — |
| `skill_group_no_agent` | broadcast | ✅ | — |
| `agent_state_changed` | broadcast | 可选 | — |
| `conference_created` | broadcast | — | — |
| `conference_member_joined` | broadcast | ✅ | +ctx |
| `conference_member_left` | broadcast | ✅ | +ctx |
| `conference_member_muted` | broadcast | ✅ | +ctx |
| `conference_member_unmuted` | broadcast | ✅ | +ctx |
| `conference_destroyed` | broadcast | — | — |
| `conference_ended_by_host` | broadcast | — | +ctx |
| `conference_error` | broadcast | — | — |
| `conference_merge_requested` | fan_out | ✅ | +ctx |
| `conference_merged` | fan_out | ✅ | +ctx |
| `conference_merge_failed` | fan_out | ✅ | +ctx |
| `conference_seat_replace_started` | fan_out | ✅ | — |
| `conference_seat_replace_succeeded` | fan_out | ✅ | — |
| `conference_seat_replace_failed` | fan_out | ✅ | — |
| `conference_seat_replace_rollback_failed` | fan_out | ✅ | — |
| `supervisor_listen_started` | owner | — | — |
| `supervisor_whisper_started` | owner | — | — |
| `supervisor_barge_started` | owner | — | — |
| `supervisor_takeover_started` | owner | — | — |
| `supervisor_mode_stopped` | owner | — | — |
| `sip_message_received` | owner | ✅ | +ctx |
| `sip_notify_received` | owner | ✅ | +ctx |

---

## 8. 开发者示例

### Python Webhook 接收

```python
from http.server import HTTPServer, BaseHTTPRequestHandler
import json

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length))

        event_type = body["event_type"]
        call_id = body["call_id"]

        print(f"[{event_type}] call_id={call_id}")

        if event_type == "recording_metadata_available":
            meta = body["event"]  # 与 WS 事件相同的扁平负载
            print(f"  download: {meta['download_url']}")
            print(f"  file_size: {meta['file_size']}")

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')

HTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
```

### Python WebSocket 实时监听

```python
import asyncio, json
from websockets import connect

async def main():
    async with connect(
        "ws://pbx.example.com/rwi/v1",
        additional_headers={"Authorization": "Bearer your-token"},
        subprotocols=["rwi-v1"],
    ) as ws:
        await ws.send(json.dumps({
            "rwi": "1.0",
            "action_id": "sub-001",
            "action": "session.subscribe",
            "params": {"contexts": ["*"]}
        }))

        async for msg in ws:
            payload = json.loads(msg)
            for key, data in payload.items():
                if key == "rwi":
                    continue
                print(f"[{key}] {json.dumps(data, ensure_ascii=False)}")

asyncio.run(main())
```

---

## 9. 辅助结构体

以下结构体供嵌套引用，不独立作为事件发出。

### IvrNodeInfo

| 字段 | 类型 | 说明 |
|------|------|------|
| `node_id` | String | 节点 ID |
| `node_name` | String | 节点名称 |
| `node_type` | String | 节点类型 |
| `routing_target` | Option\<String\> | 路由目标 |
| `previous_node_id` | Option\<String\> | 上一节点 ID |
| `next_node_id` | Option\<String\> | 下一节点 ID |
| `duration_ms` | Option\<u32\> | 停留时长 |
| `result_value` | Option\<String\> | 按键/结果 |

### IvrFlowContext

| 字段 | 类型 | 说明 |
|------|------|------|
| `app_id` | String | IVR 应用 ID |
| `routing_path` | Vec\<String\> | 路由路径 |
| `service_type` | Option\<String\> | 业务类型 |
| `customer_type` | Option\<String\> | 客户类型 |

---

**文档版本**：v1.0  
**最后更新**：2026-06-23  
**代码来源**：`src/rwi/event.rs`
