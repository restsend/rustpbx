# RustPBX WebSocket 接口（RWI）

> 译者注：本篇是英文原文的完整译文，代码、配置和示例注释原样保留。原文混用点号和下划线事件名，且对 RwiApp、contexts、接管和权限的部分描述与此前核对的实现不一致；这些内容没有在翻译中擅自改写。第 12 节明确为未实现的设计提案，不可当作可用配置。

RWI 是 RustPBX 呼叫编排的 JSON-over-WebSocket 控制面。它沿用 Asterisk AMI 的动作/事件模型，并将其适配为基于 WebSocket 的 JSON，以支持现代呼叫中心工作流。

## 1. 概述

RWI 提供：

- **命令通道**：客户端 → RustPBX。
- **事件通道**：RustPBX → 客户端。

RWI 不替代 SIP 信令。它通过 RustPBX 内部的 `CallController` 和 `SessionAction` 抽象控制通话行为。

## 2. 架构

RWI 作为 RustPBX 内置模块实现，核心组成包括：

- **RwiGateway**：维护已认证的 WS 会话，按 `call_id` 路由命令/事件。
- **RwiApp**（每通通话一个）：将 CallApp 事件转换为 RWI 事件，在 `CallController` 上执行已验证的 RWI 命令。

## 3. 认证

在 WebSocket 升级阶段通过 HTTP 头认证，不需要 `Login` 动作。

```
GET /rwi/v1 HTTP/1.1
Upgrade: websocket
Authorization: Bearer <ami_token>
```

也可以通过查询参数传 Token（适用于不能设置请求头的客户端）：

```
GET /rwi/v1?token=<ami_token> HTTP/1.1
Upgrade: websocket
```

Token 缺失或无效时，服务器在 WebSocket 握手完成前以 `HTTP 401 Unauthorized` 拒绝升级。

Token 静态配置在 `[rwi.tokens]` 中，每个 Token 携带一组权限范围。

## 4. 协议

### 4.1 消息封装

所有字段统一使用 snake_case：

| 字段 | 方向 | 用途 |
|-------|-----------|---------|
| `rwi` | 双向 | 协议版本（可选，供未来兼容使用） |
| `action` | 客户端 → 服务器 | 命令名称（必填） |
| `action_id` | 客户端 → 服务器 | 客户端生成的关联 ID（必填） |
| `event` | 服务器 → 客户端 | 异步推送事件名 |

注意：`rwi` 字段可选，目前被忽略。版本已经编码在 WebSocket URL 路径（`/rwi/v1`）中。

### 4.2 异步命令模型

RWI 使用完全异步的事件驱动模型（类似 FreeSWITCH ESL）。**所有命令通过事件接收结果**，没有同步响应。

**命令流程：**

1. 客户端发送携带 `action_id` 的命令。
2. 服务器验证并异步执行。
3. 服务器发送携带匹配 `action_id` 的 `command_completed` 或 `command_failed` 事件。
4. 客户端通过 `action_id` 关联响应。

**客户端命令格式：**

```json
{
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "params": {
    "call_id": "c_92f4"
  }
}
```

**命令完成事件：**

```json
{
  "type": "command_completed",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "call_id": "c_92f4",
  "status": "success"
}
```

**带数据结果的命令：**

```json
{
  "type": "command_completed",
  "action_id": "req-001",
  "action": "call.originate",
  "status": "success",
  "data": {
    "call_id": "c_92f4"
  }
}
```

**命令失败事件：**

```json
{
  "type": "command_failed",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "call_id": "c_92f4",
  "error": "Call not found: c_92f4"
}
```

**服务器异步事件（无 action_id）：**

```json
{
  "event": "call.incoming",
  "call_id": "c_92f4",
  "data": {
    "context": "default",
    "caller": "1001",
    "callee": "2000",
    "direction": "inbound",
    "trunk": "trunk_main",
    "sip_headers": {
      "X-Tenant-ID": "corp_a",
      "P-Asserted-Identity": "sip:1001@pbx.local"
    }
  }
}
```

### 4.3 命令格式

RWI 命令使用 JSON 带标签联合格式。`action` 标识命令类型，`params` 包含命令专属参数：

```json
{
  "action": "call.originate",
  "action_id": "req-001",
  "params": {
    "call_id": "leg_a",
    "destination": "sip:bob@local",
    "caller_id": "4000",
    "timeout_secs": 30
  }
}
```

部分命令支持便捷别名：

| 主名称 | 别名 |
|-------------|-------|
| `session.subscribe` | `Subscribe` |
| `call.originate` | `Originate` |
| `call.answer` | `Answer` |
| `media.play` | `MediaPlay` |

## 5. 命令参考

### 5.1 会话命令

| 命令 | 说明 |
|---------|-------------|
| `session.subscribe` | 订阅一个或多个上下文 |
| `session.unsubscribe` | 取消上下文订阅 |
| `session.list_calls` | 列出此会话拥有的通话 |
| `session.attach_call` | 附着到已有通话（督导模式） |
| `session.detach_call` | 释放通话控制权或督导 |
| `session.resume` | 从指定序号恢复事件流 |

**订阅示例：**

```json
{
  "action": "session.subscribe",
  "action_id": "req-s01",
  "params": {
    "contexts": ["ivr_bot", "queue_overflow"]
  }
}
```

### 5.2 通话控制命令

| 命令 | 说明 |
|---------|-------------|
| `call.originate` | 发起外呼 |
| `call.answer` | 接听 |
| `call.reject` | 拒绝 |
| `call.ring` | 发送振铃 |
| `call.hangup` | 挂断 |
| `call.bridge` | 桥接两通通话 |
| `call.unbridge` | 解除通话桥接 |
| `call.transfer` | 转接（盲转） |
| `call.transfer.replace` | 替换目标对话进行转接（RFC 3891 Replaces） |
| `call.transfer.attended` | 咨询转接（先咨询） |
| `call.transfer.complete` | 完成咨询转接 |
| `call.transfer.cancel` | 取消咨询转接 |
| `call.hold` | 保持通话（可带音乐） |
| `call.unhold` | 取消保持 |
| `call.set_ringback_source` | 设置回铃音来源 |
| `call.set_var` / `call.get_var` | 设置/读取通话变量 |
| `call.send_dtmf` | 向远端发送 DTMF 数字 |
| `dtmf.collect` | 收集多位 DTMF 字符串 |
| `call.leg_add` / `call.leg_remove` | 动态添加/移除通话腿 |
| `call.app_start` / `call.app_stop` | 在会话上启动/停止通话应用 |
| `call.resume` | 重新附着后恢复接收某通通话事件 |
| `app.chain` | 串联进入另一个通话应用 |

**发起呼叫：**

```json
{
  "action": "call.originate",
  "action_id": "req-001",
  "params": {
    "call_id": "leg_a",
    "destination": "sip:bob@local",
    "caller_id": "4000",
    "timeout_secs": 30,
    "extra_headers": {
      "X-Campaign-ID": "camp_001"
    }
  }
}
```

**桥接两通通话：**

```json
{
  "action": "call.bridge",
  "action_id": "req-003",
  "params": {
    "leg_a": "leg_a",
    "leg_b": "leg_b"
  }
}
```

**拒绝通话：**

```json
{
  "action": "call.reject",
  "action_id": "req-010",
  "params": {
    "call_id": "c_92f4",
    "reason": "busy"
  }
}
```

有效 `reason` 值：`busy`、`forbidden`、`not_found`。

### 5.3 媒体命令

| 命令 | 说明 |
|---------|-------------|
| `media.play` | 播放音频文件 |
| `media.stop` | 停止播放 |
| `call.transfer` → `voip_bridge:` / `bridge:` | 通过 WebSocket 双向传输 PCM16（见下文） |

**实时双向 PCM（voip_bridge）：**

`media.stream_start` / `media.inject_start` 已移除。当前支持的实时双向 PCM 方式是将通话（或通话腿）转接至 `voip_bridge:` / `bridge:` WebSocket 端点。PBX 主动连接给定的 `ws(s)://` URL，双向传输小端 PCM16 二进制帧（DTMF 作为 JSON 文本帧转发）。同时适用于**呼入和主动外呼**；在 `call_answered` 后转接 `caller` 腿：

```json
{
  "action": "call.transfer",
  "action_id": "req-012",
  "params": {
    "call_id": "c_92f4",
    "target": "voip_bridge:ws://media.example.com:9000/ws?samplerate=8000&codec=pcm"
  }
}
```

可选查询参数：`samplerate`（默认 8000）、`codec`、`timeout_ms`、`_hdr_<name>`（自定义头）、`return_app` / `return_target`（桥接断开后通话返回的位置）。

**播放音频：**

```json
{
  "action": "media.play",
  "action_id": "req-011",
  "params": {
    "call_id": "c_92f4",
    "source": {
      "type": "file",
      "uri": "sounds/welcome.wav"
    },
    "interrupt_on_dtmf": true
  }
}
```

**媒体来源类型：**

```json
{ "type": "file", "uri": "sounds/hold.wav", "looped": true }
{ "type": "silence" }
{ "type": "ringback" }
```

### 5.4 录音命令

| 命令 | 说明 |
|---------|-------------|
| `record.start` | 开始录音 |
| `record.pause` | 暂停录音 |
| `record.resume` | 恢复录音 |
| `record.stop` | 停止录音 |

**开始录音：**

```json
{
  "action": "record.start",
  "action_id": "req-020",
  "params": {
    "call_id": "c_92f4",
    "mode": "mixed",
    "beep": false,
    "max_duration_secs": 7200,
    "storage": {
      "path": "records/2026/03/13/c_92f4.wav"
    }
  }
}
```

`record.start` 同时适用于呼入**和主动外呼**。不论拨号计划的自动录音标志如何，显式开始都会生效。录音结束（显式停止或挂断）时，所有者收到 `record_stopped`，CDR 携带录音文件（→ `recording_url`）。启用 `[recording]` 时，还会发出 `recording_metadata_available` 和 `record_end`。

**外呼时录音（媒体建立时自动开始）：** `call.originate`（以及 `POST /ami/v1/outbound/dial`）接受与 `record.start` 形状相同的 `record` 对象。第一个远端 SDP 建立媒体时自动开始录音（例如 183 临时响应或最终应答）。空 `storage.path` 使用默认位置（`[recording].path/<call_id>.wav`）：

```json
{
  "action": "call.originate",
  "action_id": "req-021",
  "params": {
    "call_id": "c_out1",
    "destination": "sip:1002@pbx.local",
    "caller_id": "sip:1000@pbx.local",
    "record": { "mode": "mixed", "beep": false, "storage": { "path": "" } }
  }
}
```

有效 `mode` 值：

- `mixed`：将两个通话方向混合为单声道 WAV。
- `separate_legs`：将两个方向分别写入立体声的两个声道。

### 5.5 队列命令

| 命令 | 说明 |
|---------|-------------|
| `queue.enqueue` | 加入队列 |
| `queue.dequeue` | 移出队列 |
| `queue.hold` | 在队列中保持 |
| `queue.unhold` | 取消队列保持 |
| `queue.set_priority` | 设置优先级 |
| `queue.assign_agent` | 分配坐席 |
| `queue.requeue` | 重新排队 |

**入队：**

```json
{
  "action": "queue.enqueue",
  "action_id": "req-030",
  "params": {
    "call_id": "c_92f4",
    "queue_id": "support_l1",
    "priority": 5
  }
}
```

### 5.6 督导命令

| 命令 | 说明 |
|---------|-------------|
| `supervisor.listen` | 静默监听 |
| `supervisor.whisper` | 仅向坐席耳语 |
| `supervisor.barge` | 加入双方通话 |
| `supervisor.takeover` | 接管（替换坐席） |
| `supervisor.stop` | 停止督导模式 |

**耳语：**

```json
{
  "action": "supervisor.whisper",
  "action_id": "req-040",
  "params": {
    "supervisor_call_id": "sup_001",
    "target_call_id": "c_92f4",
    "agent_leg": "a_leg"
  }
}
```

### 5.7 SIP 消息命令

| 命令 | 说明 |
|---------|-------------|
| `sip.message` | 发送 SIP MESSAGE |
| `sip.notify` | 发送 SIP NOTIFY |
| `sip.options_ping` | SIP OPTIONS 探测 |

**发送 SIP MESSAGE：**

```json
{
  "action": "sip.message",
  "action_id": "req-msg-01",
  "params": {
    "call_id": "c_92f4",
    "content_type": "text/plain",
    "body": "Your ticket number is 12345"
  }
}
```

### 5.8 会议命令

| 命令 | 说明 |
|---------|-------------|
| `conference.create` | 创建会议 |
| `conference.add` | 将通话加入会议 |
| `conference.remove` | 将通话移出会议 |
| `conference.mute` | 参与者静音 |
| `conference.unmute` | 取消参与者静音 |
| `conference.destroy` | 销毁会议 |
| `conference.end` | 主持人结束会议（移除所有参与者） |
| `conference.merge` | 将咨询腿合并入会议 |
| `conference.seat_replace` | 原子地用另一参与者替换当前参与者 |

**创建会议：**

```json
{
  "action": "conference.create",
  "action_id": "req-conf-01",
  "params": {
    "conf_id": "room_42",
    "max_members": 10
  }
}
```

**席位替换（A → A1）：**

```json
{
  "action": "conference.seat_replace",
  "action_id": "req-conf-seat-01",
  "params": {
    "conference_id": "room_42",
    "old_call_id": "call_a",
    "new_call_id": "call_a1"
  }
}
```

## 6. 事件参考

### 6.1 命令结果事件

| 事件 | 说明 |
|-------|-------------|
| `command_completed` | 命令执行成功（包含 `action_id`、`action` 和可选 `data`） |
| `command_failed` | 命令执行失败（包含 `action_id`、`action`、`error`） |

**包含数据的命令完成事件：**

```json
{
  "type": "command_completed",
  "action_id": "req-001",
  "action": "call.originate",
  "call_id": "c_92f4",
  "status": "success",
  "data": {
    "call_id": "c_92f4"
  }
}
```

### 6.2 通话事件

| 事件 | 说明 |
|-------|-------------|
| `call.incoming` | 来电到达（派发给订阅的上下文） |
| `call.ringing` | 远端正在振铃（出站腿收到 180） |
| `call.early_media` | 远端发送带 SDP 的 183（早期媒体/回铃音透传已启用） |
| `call.answered` | 通话已接听（200 OK） |
| `call.bridged` | 两条腿已桥接 |
| `call.unbridged` | 桥接已拆除 |
| `call.transferred` | 已通过 REFER 发起转接 |
| `call.transfer.accepted` | REFER 目标接受 |
| `call.transfer.failed` | REFER 目标失败 |
| `call.hangup` | 通话结束 |
| `call.no_answer` | 出站腿超时 |
| `call.busy` | 出站腿返回 486 Busy |

### 6.3 媒体事件

| 事件 | 说明 |
|-------|-------------|
| `media.hold.started` | 保持音乐开始 |
| `media.hold.stopped` | 保持音乐停止 |
| `media.ringback.passthrough.started` | 正在向目标腿转发 183 早期媒体 |
| `media.play.started` | 开始播放 |
| `media.play.finished` | 播放结束 |

### 6.4 录音事件

| 事件 | 说明 |
|-------|-------------|
| `record.started` | 录音开始 |
| `record.paused` | 录音暂停 |
| `record.resumed` | 录音恢复 |
| `record.stopped` | 录音停止 |
| `record.failed` | 录音失败 |

### 6.5 队列事件

| 事件 | 说明 |
|-------|-------------|
| `queue.joined` | 已加入队列 |
| `queue.position_changed` | 排队位置变化 |
| `queue.agent_offered` | 向坐席分配来电 |
| `queue.agent_connected` | 坐席已连接 |
| `queue.left` | 已离开队列 |
| `queue.wait_timeout` | 等待超时 |

### 6.6 督导事件

| 事件 | 说明 |
|-------|-------------|
| `supervisor.listen.started` | 监听开始 |
| `supervisor.whisper.started` | 耳语开始 |
| `supervisor.barge.started` | 强插开始 |
| `supervisor.mode.stopped` | 督导模式停止 |
| `supervisor.takeover.started` | 督导接管了坐席腿 |

### 6.7 SIP 事件

| 事件 | 说明 |
|-------|-------------|
| `sip.message.received` | 收到 SIP MESSAGE |
| `sip.notify.received` | 收到 SIP NOTIFY |
| `dtmf` | DTMF 数字 |

### 6.8 会议事件

| 事件 | 说明 |
|-------|-------------|
| `conference.created` | 会议已创建 |
| `conference.member.joined` | 成员加入 |
| `conference.member.left` | 成员离开 |
| `conference.member.muted` | 成员静音 |
| `conference.member.unmuted` | 取消成员静音 |
| `conference.destroyed` | 会议已销毁 |
| `conference.error` | 会议错误 |
| `conference.seat_replace.started` | 席位替换事务开始 |
| `conference.seat_replace.succeeded` | 席位替换成功完成 |
| `conference.seat_replace.failed` | 席位替换失败（已尝试回滚） |
| `conference.seat_replace.rollback_failed` | 替换失败后回滚也失败 |

### 6.9 席位替换事件顺序

除了成员加入/离开事件，服务器还发出明确的席位替换生命周期事件。

成功路径：

1. `conference_seat_replace_started`
2. `conference_member_left`（原席位）
3. `conference_member_joined`（新席位）
4. `conference_seat_replace_succeeded`

失败路径（带回滚）：

1. `conference_seat_replace_started`
2. `conference_member_left`（原席位）
3. `conference_member_joined`（原席位回滚）
4. `conference_seat_replace_failed`

失败路径（回滚也失败）：

1. `conference_seat_replace_started`
2. `conference_member_left`（原席位）
3. `conference_seat_replace_rollback_failed`
4. `conference_seat_replace_failed`

## 7. 错误处理

命令失败通过 `command_failed` 事件报告：

```json
{
  "type": "command_failed",
  "action_id": "req-001",
  "action": "call.answer",
  "call_id": "c_92f4",
  "error": "Call not found: c_92f4"
}
```

常见错误消息：

| 错误模式 | 说明 |
|---------------|-------------|
| `Call not found: <id>` | 通话 ID 不存在 |
| `Command failed: <reason>` | 通用命令执行失败 |
| `Not implemented: <feature>` | 功能尚未实现 |
| `invalid state` | 当前通话状态不允许该操作 |
| `already owned` | 通话已由其他会话拥有 |

## 8. 事件类型

### 8.1 上下文订阅

RWI 支持多个客户端同时连接，每个连接独立认证。客户端订阅上下文后可以接收来电事件。

- **Context（上下文）**：将来电映射到相关客户端的路由标签。
- **Ownership（控制权）**：每通活动通话同一时刻恰有一个控制客户端，只有所有者可以发出控制动作。
- **Fan-out（扇出）**：`call.incoming` 投递给所有订阅了匹配上下文的客户端；控制权由先成功认领者取得。

### 8.2 来电派发流程

```
1. SIP INVITE → RustPBX proxy
2. Dialplan routing resolves: app=rwi, context="ivr_bot"
3. RustPBX creates RwiApp for the call, holds it in ringing state
4. RwiGateway fans out call.incoming to ALL clients subscribed to "ivr_bot"
5. Client(s) receive call.incoming and may call.answer / call.reject to claim
6. First valid call.answer wins → that client becomes owner
7. If no client responds within no_answer_timeout_secs:
   → server executes no_answer_action (hangup, transfer, or play tone)
```

### 8.3 外呼控制权

通过 `call.originate` 发起的通话立即归发起客户端所有，无需订阅或附着步骤。

## 9. 配置

```toml
[rwi]
max_connections = 2000
max_calls_per_connection = 200
orphan_hold_secs = 30
originate_rate_limit = 10

# AMI tokens — no login action required
[[rwi.tokens]]
token = "secret-control-token"
scopes = ["call.control", "queue.control", "record.control"]

[[rwi.tokens]]
token = "secret-supervisor-token"
scopes = ["call.control", "supervisor.control", "media.stream"]

[[rwi.tokens]]
token = "secret-bot-token"
scopes = ["call.control", "media.stream"]

# Contexts define how inbound calls are dispatched to RWI clients
[[rwi.contexts]]
name = "ivr_bot"
no_answer_timeout_secs = 10
no_answer_action = "hangup"

[[rwi.contexts]]
name = "queue_agent_1"
no_answer_timeout_secs = 30
no_answer_action = "transfer"
no_answer_transfer_target = "sip:voicemail@local"
```

## 10. 安全

1. **认证**：
   - WebSocket 升级时，通过 HTTP 头 `Authorization: Bearer <ami_token>` 传递静态 AMI Token。
   - Token 静态配置在 `[rwi.tokens]` 中。
   - 缺少或无效 Token 的升级请求以 `HTTP 401 Unauthorized` 拒绝。

2. **授权**：
   - 每个 Token 的 RBAC 范围（`call.control`、`queue.control`、`supervisor.control`、`media.stream`）。
   - 每次操作检查通话所有权。

3. **传输**：
   - 生产环境使用 `wss`。
   - 可选 mTLS 支持。

## 11. 命令实现状态

> 最后更新：2026-08-14（英文原文标注日期）。

### 图例

- ✅ **完整实现**——命令功能完整。
- ⚠️ **部分实现**——命令可用，但有限制。
- 🔧 **占位/TODO**——接受命令，但实际功能未完成。

### 按类别列出的实现状态

| 类别 | 状态 | 备注 |
|----------|--------|-------|
| **会话命令** | ✅ 完整 | 所有会话命令完整实现 |
| **通话控制** | ✅ 完整 | 外呼、接听、挂断、桥接、转接均可用 |
| **媒体播放** | ✅ 完整 | 播放、停止、保持音乐功能完整 |
| **录音** | ✅ 完整 | 已实现开始、暂停、恢复、停止；`call.originate` / `outbound/dial` 的内嵌 `record`（首个远端 SDP 时自动开始）；CDR 携带文件 → `recording_metadata_available` + `record_end` |
| **队列** | ✅ 完整 | 入队、出队、保持、取消保持可用 |
| **督导** | ⚠️ 部分 | 命令已实现，**实际音频混音 TODO** |
| **会议** | ⚠️ 部分 | 创建/添加/移除/销毁可用，**混音器中的静音/取消静音 TODO** |
| **媒体流/注入** | ➖ 已替代 | 命令已移除，使用 `call.transfer` → `voip_bridge:` 实现双向 PCM |
| **SIP 消息** | ✅ 完整 | `sip.message` / `sip.notify` / `sip.options_ping` 构造并发送实际 SIP 请求 |

### 已知限制

1. **轨道静音**：通过 Console REST API 提供（`/api/calls/{id}/command` 的 `mute` / `unmute` 动作），不通过 RWI WebSocket 提供。
2. **会议静音**：`conference.mute` / `conference.unmute` 发出事件，但没有真正静音混音器音频。
3. **PCM 流**：`media.stream_start` / `media.inject_start` 已移除。通过 `call.transfer` 到 `voip_bridge:` / `bridge:` WebSocket 端点提供实时双向 PCM，支持呼入和外呼。
4. **SDP 重新协商**：保持/re-INVITE 的 SDP 重新协商仍为 TODO。

---

## 12. 智能路由与规则引擎（设计提案——未实现）

> **状态**：本章描述的 `[rwi.smart_routing]` / `[rwi.local_rules]` / `[rwi.sip_header_passthrough]` 配置是设计提案。当前代码不解析也不执行这些内容，这些 TOML 节会被静默忽略。不要在生产环境依赖它们。

RWI 支持面向高可靠呼叫中心场景的智能对话内消息路由和本地规则执行。

### 12.1 三层架构

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 3: RWI Application                                    │
│         Complex business logic, real-time AI decision        │
├─────────────────────────────────────────────────────────────┤
│ Layer 2: Local Rule Engine                                  │
│         Fallback rules when RWI disconnected                 │
│         Hotkey-triggered local actions                       │
├─────────────────────────────────────────────────────────────┤
│ Layer 1: Realtime Processing (SIP/RTP)                      │
│         DTMF auto-forward, INFO/OPTIONS passthrough          │
│         <10ms latency, always available                      │
└─────────────────────────────────────────────────────────────┘
```

### 12.2 消息路由配置

```toml
[rwi.smart_routing]
enabled = true

# DTMF handling
[rwi.smart_routing.dtmf]
handling = "smart_forward"  # passthrough, local_rules, smart_forward, rwi_controlled
log_to_cdr = true

[[rwi.smart_routing.dtmf.hotkeys]]
sequence = "*9"
action = "forward_rwi"      # forward_leg, forward_rwi, execute_rule, auto_reply, drop

[[rwi.smart_routing.dtmf.hotkeys]]
sequence = "*0"
action = "execute_rule"
rule_id = "emergency_escalation"

# In-dialog INFO/OPTIONS/MESSAGE routing
[rwi.smart_routing.in_dialog]
enabled = true
notify_rwi = true           # Notify RWI even when forwarding

[[rwi.smart_routing.in_dialog.rules]]
name = "Route INFO to RWI"
priority = 100
enabled = true
method = "INFO"
content_type = "application/*"
action = { type = "forward_rwi", wait_response = true, timeout_ms = 5000 }

[[rwi.smart_routing.in_dialog.rules]]
name = "Auto-reply OPTIONS"
priority = 200
enabled = true
method = "OPTIONS"
action = { type = "auto_reply", code = 200 }
```

### 12.3 DTMF 处理模式

| 模式 | 行为 | 用途 |
|------|----------|----------|
| `passthrough` | 将所有 DTMF 转发给对端 | 默认，延迟最小 |
| `local_rules` | 仅执行本地规则 | 自包含 IVR |
| `smart_forward` | 透传 + 热键检测 | 带热键的呼叫中心 |
| `rwi_controlled` | 缓冲并转发到 RWI | 复杂多位输入 |

### 12.4 本地规则引擎

RWI 断开，或触发 `action = "execute_rule"` 时，本地规则引擎执行预定义动作：

**可用动作：**

- `originate`——发起新通话。
- `bridge`——桥接到另一通通话。
- `hangup`——挂断，可附带原因。
- `play_prompt`——播放音频文件。
- `send_dtmf`——向对端发送 DTMF。
- `conference_add`——加入会议。
- `sequence`——按顺序执行多个动作。
- `conditional`——根据条件分支。

**规则示例：**

```toml
[[rwi.local_rules]]
id = "emergency_escalation"
enabled = true

[[rwi.local_rules.actions]]
action = "play_prompt"
audio_file = "sounds/transferring.wav"

[[rwi.local_rules.actions]]
action = "originate"
destination = "sip:supervisor@backup-pbx.local"
caller_id = "Emergency Hotkey"
timeout_secs = 30
```

### 12.5 优雅降级

RWI 连接丢失时：

1. 活动通话继续（第 1 层）。
2. 新事件自动执行回退规则（第 2 层）。
3. RWI 重连后可以恢复通话控制。

```toml
[rwi.smart_routing.fallback]
when_rwi_disconnected = "execute_rules"  # passthrough, execute_rules, auto_hangup
rules = ["maintain_call", "log_cdr"]
```

## 13. 限制与说明

1. **SIP 头透传**：`call.incoming` 中的 `sip_headers` 为只读；设计章节的 `[rwi.sip_header_passthrough]` 白名单不存在，INVITE 头按采集时的内容转发。
2. **PCM 流**：通过 `call.transfer` → `voip_bridge:` WebSocket 端点提供（双向 PCM16 二进制帧）；旧 `media.stream_start` / `media.inject_start` 命令已移除。
3. **外部 MCU**：外部会议后端需要集成 SIP MCU 服务器。
4. **在线状态**：RWI 不管理坐席在线状态，使用单独的 Presence 服务。
5. **督导音频**：MediaMixer 框架已就位，但实际音频流混音尚未接通。
6. **3PCC 外呼**：TransferController 的 3PCC 回退与外呼集成仍为 TODO（代码中有标记）。
