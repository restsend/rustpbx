# RWI Events 开发者参考

> 代码来源：`src/rwi/proto.rs` ｜ 协议版本：`1.0`

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
events = ["call_hangup", "record_stopped", "dn_state_changed"]   # 空 = 全部事件
```

---

## 3. 信封格式

### WebSocket 事件

```json
{
  /* 事件数据字段直接扁平化到顶层，无额外包裹 */
}
```

示例：
```json
{
  "call_id": "call-abc123",
  "caller_name": "330909",
  "callee_name": "9242000001",
  "direction": "inbound"
}
```

> WebSocket 事件以事件字段直接作为 JSON 顶层键值，不含 `"rwi"` 或事件类型名称包裹。客户端通过连接时协商的订阅规则识别事件类型。

### Webhook 信封

```json
{
  "rwi": "1.0",
  "sequence": 42,
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
| `sequence` | u64 | 单调递增事件序号，用于去重和断线重连 |
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
| `agent_id` | Option\<String\> | 坐席 ID |
| `agent_name` | Option\<String\> | 坐席名称 |

**说明**：
- `ani` vs `caller`：`ani` 是纯号码（用于业务匹配），`caller` 是完整 SIP URI
- `dnis` vs `callee`：同上
- 上下文由 `CallMetaStore` 在 gateway 分发时自动注入，事件生产者无需手动填充

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
  "params": { "last_sequence": 42 }
}
```

服务端缓存最近 1000 条事件（保留 60 秒），重连后自动回放 `last_sequence` 之后的事件。

### Webhook 去重

Webhook 使用 `(call_id, sequence)` 元组去重，环形缓冲区容量 4096 条。重复事件自动丢弃。

---

## 6. 完整事件字典

> 下方各表 `+ctx` 表示该事件携带扁平化上下文字段。
> `?` 表示 `Option<T>` 字段，值为 `null` 时省略。

### 6.1 呼叫生命周期

#### call_incoming

分发：fan_out_to_context

新呼叫进入系统，是任何呼叫流程的第一个事件。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫唯一标识 |
| `context` | String | 拨号计划 context |
| `caller` | String | 主叫 SIP URI |
| `callee` | String | 被叫 SIP URI |
| `dial_direction` | String | `inbound` / `outbound` / `internal` |
| `trunk` | Option\<String\> | SIP 中继名 |
| `sip_headers` | Map\<String, String\> | 白名单 SIP 头 |
| `root_call_id` | Option\<String\> | 根呼叫 ID（转接中不变） |
| `caller_name` | Option\<String\> | 主叫号码 |
| `callee_name` | Option\<String\> | 被叫号码 / DNIS |
| `called_phone` | Option\<String\> | 实际被叫号码（外呼场景） |
| `app_id` | Option\<String\> | IVR 应用 ID |
| `routing_target` | Option\<String\> | 路由目标 |
| `uuid` | Option\<String\> | 全局 UUID（关联录音） |
| `routing_path` | Option\<Vec\<String\>\> | 路由路径序列 |

> **注意**：`call_incoming` 使用 `dial_direction`，其他事件的上下文使用 `direction`。

```json
{
  "rwi": "1.0",
  "call_incoming": {
    "call_id": "call-abc",
    "context": "inbound",
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "dial_direction": "inbound",
    "trunk": "trunk_sip",
    "sip_headers": { "X-Tenant": "corp_a" },
    "root_call_id": "call-root-42",
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
| *+ctx* | | 扁平化上下文 |

#### call_transfer_failed

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `sip_status` | Option\<u16\> | SIP 状态码 |
| `reason` | Option\<String\> | 失败原因 |
| *+ctx* | | 扁平化上下文 |

### 6.3 媒体事件

#### media_hold_started / media_hold_stopped / media_stream_started / media_stream_stopped

分发：call_owner

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| *+ctx* | | 扁平化上下文 |

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
| `root_call_id` | Option\<String\> | 根呼叫 ID |

> 注意：`record_stopped` 不携带扁平化上下文，但自身已包含 `ani`/`dnis` 等字段，`enrich()` 会从上下文补充 `None` 字段。

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
    "root_call_id": "call-root-42"
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

**RecordingMetadata 字段**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `filename` | String | 录音文件名（必填） |
| `unique_id` | String | 录音 UUID（必填） |
| `file_size` | u64 | 文件大小字节（必填） |
| `download_url` | Option\<String\> | 下载地址 |
| `caller_name` | Option\<String\> | 主叫号码 |
| `callee_name` | Option\<String\> | 被叫号码 |
| `called_phone` | Option\<String\> | 实际被叫号码 |
| `call_type` | String | 呼叫类型（必填） |
| `agent_id` | Option\<String\> | 坐席 ID |
| `agent_name` | Option\<String\> | 坐席名称 |
| `call_start_time` | Option\<String\> | 通话开始时间 |
| `call_end_time` | Option\<String\> | 通话结束时间 |
| `upload_time` | Option\<String\> | 上传完成时间 |
| `switch_flag` | Option\<String\> | 站点标识 |
| `process_flag` | Option\<String\> | 处理进程标识（如 `ks_22_normal`） |
| `root_call_id` | Option\<String\> | 根呼叫 ID |

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
      "root_call_id": "call-root-42"
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

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `node_id` | String | 节点 ID |
| `node_name` | String | 节点名称 |
| `result_value` | Option\<String\> | 用户按键或分支结果 |
| `duration_ms` | u32 | 节点停留时长（毫秒） |
| `exit_time` | String | 退出时间 |
| `next_node_id` | Option\<String\> | 下一个节点 ID |
| `hangup_reason` | Option\<String\> | 挂机原因 |
| `call_result` | Option\<String\> | 通话结果 |
| *+ctx* | | 扁平化上下文 |

#### ivr_flow_transitioned

分发：fan_out_to_context

呼叫在 IVR 应用之间跳转。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `from_app_id` | String | 源应用 ID |
| `to_app_id` | String | 目标应用 ID |
| `from_node_id` | String | 源节点 ID |
| `to_node_id` | String | 目标节点 ID |
| `transition_reason` | String | 跳转原因（`menu_choice`、`transfer`、`overflow` 等） |
| `transition_time` | String | 跳转时间 |
| `next_routing_target` | Option\<String\> | 下一个路由目标 |
| *+ctx* | | 扁平化上下文 |

#### ivr_flow_completed

分发：fan_out_to_context

IVR 流程完成（执行了终止动作：转接、排队、留言、挂机）。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `app_id` | String | IVR 应用 ID |
| `total_nodes_traversed` | u32 | 经过的节点总数 |
| `total_duration_ms` | u32 | IVR 总耗时（毫秒） |
| `final_result` | String | 最终结果（`transferred`、`voicemail`、`abandoned` 等） |
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

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `session_id` | String | 会话 ID |
| `caller` | String | 主叫 |
| `callee` | String | 被叫 |
| `step_index` | u32 | 步骤序号 |
| `event_type` | String | 事件类型（如 `session_start`、`dtmf`、`audio_complete`、`action_execute`） |
| `action_type` | String | 动作类型（如 `Transfer`、`Prompt`、`DtmfMenu`） |
| `action_json` | Option\<String\> | 动作详情 JSON |
| `result_kind` | String | 结果类型（`terminal`、`continue`、`error`） |
| `duration_ms` | u64 | 步骤执行耗时（毫秒），始终有值 |
| `error` | Option\<String\> | 错误信息 |
| `step_id` | Option\<String\> | 当前节点 ID，由 Provider 通过 ActionNode.step_id 返回 |
| `step_name` | Option\<String\> | 当前节点名称，由 Provider 通过 ActionNode.step_name 返回 |
| `step_start_time` | Option\<String\> | 当前步骤开始时间（ISO UTC） |
| `step_end_time` | Option\<String\> | 当前步骤结束时间（ISO UTC）。仅步骤执行完成（terminal/error）时有值；等待用户输入（WaitFor）时为 null |
| `extra` | Option\<JSON Object\> | Provider 透传的额外数据。Provider 在每次响应的 ActionNode.extra 中返回完整对象，RustPBX 透传存储并原样输出 |

> **时间字段说明**：
> - `step_start_time` — 当前步骤的开始时间（上一步结束或 session 开始）
> - `step_end_time` — 步骤结束时间（仅完成时）
>
> **耗时字段说明**：
> - `duration_ms` — 步骤执行耗时（毫秒），始终有值，包含 provider 往返和动作执行时间

---

### 6.6 队列 / ACD 事件

所有队列事件携带扁平化上下文。

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

#### queue_overflowed

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `original_queue_id` | String | 原队列 ID |
| `overflow_queue_id` | String | 溢出目标队列 ID |
| `reason` | String | 溢出原因 |
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

### 6.8 分机（DN）事件

#### dn_state_changed

分发：broadcast

分机级别的细粒度信令事件。

| 字段 | 类型 | 说明 |
|------|------|------|
| `caller` | String | 分机号 / 主叫 |
| `event_name` | String | 事件名称（见下表） |
| `system_time` | String | 系统时间 |
| `call_id` | Option\<String\> | 关联呼叫 ID |
| `agent_id` | Option\<String\> | 坐席 ID |
| `caller_name` | Option\<String\> | 主叫名称/号码 |
| `callee_name` | Option\<String\> | 被叫名称/号码 |
| `reason_code` | Option\<String\> | 原因码 |
| `agent_work_mode` | Option\<String\> | 坐席工作模式 |
| `releasing_party` | Option\<String\> | 释放方（`"1 Local"` / `"2 Remote"`） |
| `vq_name` | Option\<String\> | 虚拟队列名 |
| `routing_target` | Option\<String\> | 路由目标 |
| `skill_group` | Option\<String\> | 技能组 |
| `extra` | Option\<Map\<String, Value\>\> | 扩展字段（省略时不出现在 JSON 中） |

**event_name 枚举值**：

| event_name | 说明 | 触发场景 |
|------------|------|----------|
| `REGISTERED` | 分机注册 | SIP REGISTER 成功 |
| `DIALING` | 外呼拨号 | 坐席外呼或手工拨号 |
| `RINGING` | 振铃 | 坐席侧振铃 |
| `ESTABLISHED` | 接通 | 通话建立，坐席接起电话 |
| `RELEASED` | 释放 | 挂机或转接成功后 |
| `ABANDONED` | 挂断 | 振铃阶段用户放弃 |
| `HELD` | 保持 | 坐席保持，用户听音乐 |
| `RETRIEVED` | 取回 | 将 held 的用户取回 |
| `PARTYCHANGED` | 多方通话状态变更 | 多方通话状态变化 |
| `PARTYADDED` | 多方通话新增 | 多方通话新增一方 |
| `PARTYDELETED` | 多方通话删除 | 多方通话减少一方 |
| `AGENTLOGIN` | 坐席登录 | 坐席从离线变为在线（CC addon） |
| `AGENTLOGOUT` | 坐席登出 | 坐席从在线变为离线（CC addon） |
| `AGENTREADY` | 坐席就绪 | 坐席进入空闲状态（CC addon） |
| `AGENTNOTREADY` | 坐席未就绪 | 坐席进入忙碌/振铃/话后处理等状态（CC addon） |
| `ONHOOK` | 摘机 | 软电话摘机 |

> **注意**：使用 `event_name` 做事件路由和匹配。

```json
{
  "rwi": "1.0",
  "dn_state_changed": {
    "caller": "80001",
    "event_name": "ESTABLISHED",
    "system_time": "2026-05-14T17:54:49.003Z",
    "call_id": "call-abc",
    "agent_id": "10001",
    "caller_name": "19534519769",
    "callee_name": "39989",
    "extra": {
      "source": "KS",
      "kz_conn_id": "kc-12345",
      "user_data": { "kz_target": "39299", "kz_flowname": "CTC400Customer" }
    }
  }
}
```

#### dn_registered / dn_unregistered

分发：broadcast

| 字段 | 类型 | 说明 |
|------|------|------|
| `caller` | String | 分机号 |
| `agent_id` | Option\<String\> | 坐席 ID |
| `register_time` / `unregister_time` | String | 注册/注销时间 |

---

### 6.9 呼叫元数据事件

#### call_metadata_updated

呼叫建立后元数据更新时触发。

| 字段 | 类型 | 说明 |
|------|------|------|
| `call_id` | String | 呼叫标识 |
| `metadata` | CallMetadata | 元数据（见下表） |

**CallMetadata 字段**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `root_call_id` | Option\<String\> | 根呼叫 ID |
| `caller_name` | Option\<String\> | 主叫号码 |
| `callee_name` | Option\<String\> | 被叫号码 |
| `called_phone` | Option\<String\> | 实际被叫号码 |
| `dial_direction` | Option\<String\> | 呼叫方向 |
| `uuid` | Option\<String\> | 全局 UUID |
| `routing_path` | Option\<Vec\<String\>\> | 路由路径 |
| `app_id` | Option\<String\> | IVR 应用 ID |
| `routing_target` | Option\<String\> | 路由目标 |
| `switch_name` | Option\<String\> | 交换机名 |

```json
{
  "rwi": "1.0",
  "call_metadata_updated": {
    "call_id": "call-abc",
    "metadata": {
      "root_call_id": "call-root-42",
      "caller_name": "330909",
      "callee_name": "9242000001",
      "called_phone": "018659727661",
      "dial_direction": "inbound",
      "uuid": "uuid-abc-123",
      "routing_path": ["menu:root", "queue:level1"],
      "app_id": "ivr-support",
      "routing_target": "queue:support",
      "switch_name": "SIP_Switch_KS"
    }
  }
}
```

---

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

### 6.12 并行外呼事件

#### parallel_originate_started

| 字段 | 类型 | 说明 |
|------|------|------|
| `operation_id` | String | 操作 ID |
| `leg_count` | u32 | 并发 leg 数量 |

#### parallel_originate_leg_ringing / parallel_originate_winner / parallel_originate_leg_cancelled

| 字段 | 类型 | 说明 |
|------|------|------|
| `operation_id` | String | 操作 ID |
| `call_id` | String | Leg 呼叫 ID |
| `destination` | String | 拨打目标 |
| `reason` | String | `leg_cancelled` 专用：取消原因 |
| *+ctx* | | 扁平化上下文 |

#### parallel_originate_completed

| 字段 | 类型 | 说明 |
|------|------|------|
| `operation_id` | String | 操作 ID |
| `winning_call_id` | String | 中选呼叫 ID |

#### parallel_originate_failed

| 字段 | 类型 | 说明 |
|------|------|------|
| `operation_id` | String | 操作 ID |
| `reason` | String | 失败原因 |

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

#### session_resumed

| 字段 | 类型 | 说明 |
|------|------|------|
| `session_id` | String | 恢复的会话 ID |
| `last_sequence` | u64 | 客户端上报的最后序号 |

---

## 7. 事件类型速查表

| 事件类型 | 分发 | call_id | 上下文 |
|----------|------|---------|--------|
| `call_incoming` | fan_out | ✅ | 自有字段 |
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
| `media_ringback_passthrough_stopped` | owner | ✅ | — |
| `media_play_started` | owner | ✅ | +ctx |
| `media_play_finished` | owner | ✅ | +ctx |
| `media_stream_started` | owner | ✅ | +ctx |
| `media_stream_stopped` | owner | ✅ | +ctx |
| `record_started` | owner | ✅ | +ctx |
| `record_paused` | owner | ✅ | +ctx |
| `record_resumed` | owner | ✅ | +ctx |
| `record_stopped` | owner | ✅ | 自有字段+enrich |
| `record_failed` | owner | ✅ | +ctx |
| `recording_metadata_available` | owner | ✅ | — |
| `dtmf` | fan_out | ✅ | +ctx |
| `dtmf_collected` | owner | ✅ | +ctx |
| `dtmf_collection_timeout` | owner | ✅ | +ctx |
| `ivr_node_entered` | fan_out | ✅ | +ctx |
| `ivr_node_exited` | fan_out | ✅ | +ctx |
| `ivr_flow_transitioned` | fan_out | ✅ | +ctx |
| `ivr_flow_completed` | fan_out | ✅ | +ctx |
| `ivr_step_trace` | fan_out | ✅ | — |
| `queue_joined` | owner/broadcast | ✅ | +ctx |
| `queue_position_changed` | owner | ✅ | +ctx |
| `queue_agent_offered` | broadcast | ✅ | +ctx |
| `queue_agent_connected` | owner | ✅ | +ctx |
| `queue_left` | broadcast | ✅ | +ctx |
| `queue_wait_timeout` | owner | ✅ | +ctx |
| `queue_overflowed` | owner | ✅ | +ctx |
| `queue_voicemail_redirected` | owner | ✅ | +ctx |
| `queue_candidates_found` | owner | ✅ | +ctx |
| `queue_agent_ringing` | owner | ✅ | +ctx |
| `queue_agent_no_answer` | owner | ✅ | +ctx |
| `queue_agent_rejected` | owner | ✅ | +ctx |
| `queue_fallback_executed` | owner | ✅ | +ctx |
| `queue_alert` | broadcast | — | — |
| `agent_state_changed` | broadcast | 可选 | — |
| `dn_state_changed` | broadcast | 可选 | — |
| `dn_registered` | broadcast | — | — |
| `dn_unregistered` | broadcast | — | — |
| `call_metadata_updated` | owner | ✅ | — |
| `conference_created` | broadcast | — | — |
| `conference_member_joined` | broadcast | ✅ | +ctx |
| `conference_member_left` | broadcast | ✅ | +ctx |
| `conference_member_muted` | broadcast | ✅ | +ctx |
| `conference_member_unmuted` | broadcast | ✅ | +ctx |
| `conference_destroyed` | broadcast | — | — |
| `conference_ended_by_host` | broadcast | — | +ctx |
| `conference_auto_ended` | broadcast | — | +ctx |
| `conference_error` | broadcast | — | — |
| `conference_consult_dialing` | owner | ✅ | +ctx |
| `conference_consult_connected` | owner | ✅ | +ctx |
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
| `parallel_originate_started` | owner | — | — |
| `parallel_originate_leg_ringing` | owner | ✅ | +ctx |
| `parallel_originate_winner` | owner | ✅ | +ctx |
| `parallel_originate_leg_cancelled` | owner | ✅ | +ctx |
| `parallel_originate_completed` | owner | ✅ | — |
| `parallel_originate_failed` | owner | — | — |
| `sip_message_received` | owner | ✅ | +ctx |
| `sip_notify_received` | owner | ✅ | +ctx |
| `call_ownership_changed` | owner | ✅ | +ctx |
| `session_resumed` | owner | — | — |

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
            meta = body["event"]["recording_metadata_available"]["metadata"]
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
**最后更新**：2026-06-05  
**代码来源**：`src/rwi/proto.rs`
