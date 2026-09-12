# Step Mode IVR — 构建自定义 Provider

> 使用外部 HTTP 服务逐步控制 IVR 流程。  
> 通话事件（DTMF、超时、音频完成等）通过 POST 发往你的 URL。  
> 你返回 `ActionNode`，指定下一步动作。

> 译文说明：按英文原文完整翻译。代码、配置和协议示例（包括原注释）原样保留。

---

## 1. 快速开始

```bash
# 1. Start the reference Python provider
python3 examples/unified_ivr_provider.py 8080

# 2. Simulate a call entering the IVR
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d '{"session_id":"call_001","caller":"1001","callee":"2000","event":{"type":"session_start"}}'

# → returns: { "type": "prompt", "file": "sounds/ivr/welcome.wav", ... }

# 3. Simulate a DTMF keypress
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d '{"session_id":"call_001","caller":"1001","callee":"2000","event":{"type":"dtmf","digit":"1"}}'
```

---

## 2. 工作原理

```
Call enters IVR
      │
      ▼
POST {url}/start          ──→ your provider (session notification, fire‑and‑forget)
POST {url}  (session_start) ──→ your provider ──→ ActionNode
      │                                               │
      ▼ execute action                                │
 [play prompt]                                         │
      │                                               │
      ▼ audio complete                                 │
POST {url}  (audio_complete) ──→ your provider ────────┘
      │
      ▼
  ... repeat until terminal action (transfer / hangup / queue)
      │
      ▼
POST {url}/end            ──→ your provider (session cleanup, fire‑and‑forget)
```

| 端点 | 调用时机 | 是否需要响应？ |
|----------|-------------|-------------------|
| `POST {url}` | 每个 IVR 步骤 | ✅ `ActionNode` |
| `POST {url}/start` | 会话开始 | ❌ 发送后不等待（发送请求头，请求体为 `SessionContext`） |
| `POST {url}/end` | 会话结束（任意原因） | ❌ 发送后不等待（请求体：`{"reason": "...", "detail": "..."}`，见 §6 结束原因标签） |
| `POST {url}/fail` | 节点执行失败（仍在 IVR 中） | ✅ 恢复用 `ActionNode`（见 §8） |

---

## 3. 请求格式 — ProviderContext

```json
{
  "session_id": "call_abc123",
  "caller": "1001",
  "callee": "2000",
  "direction": "inbound",
  "tenant_id": null,
  "ivr_id": "proj_42",
  "variables": {
    "user_phone": "13800138000",
    "api_result": "{\"balance\": 100}"
  },
  "event": {
    "type": "dtmf",
    "digit": "1"
  }
}
```

### ProviderContext 字段

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `session_id` | `string` | 每通通话唯一 |
| `caller` | `string` | 主叫号码 |
| `callee` | `string` | 被叫号码 |
| `direction` | `"inbound"` 或 `"outbound"` | 通话方向 |
| `tenant_id` | `string` 或 `null` | 租户标识 |
| `ivr_id` | `string` 或 `null` | 用于追踪的 IVR 项目标识 |
| `variables` | `Map<string, string>` | 会话变量（见“变量”章节） |
| `event` | `ProviderEvent` 或 `null` | 触发此步骤的事件 |
| `transferred_from` | `string` 或 `null` | 本次会话是否由转接进入：`"ivr"`（JumpIvr / IVR 间跳转）、`"agent"`、`"queue"`；`null` 表示全新进入。为 `"ivr"` 时，`variables` 中还会附带 `source_ivr`（来源 IVR 短码）与 `source_node`（来源节点 ID） |

### ProviderEvent 类型

| `type` | 附加字段 | 触发时机 |
|--------|--------------|----------------|
| `session_start` | 无 | 通话进入 IVR |
| `dtmf` | `digit: string` | 用户按下 DTMF 键 |
| `dtmf_timeout` | 无 | 按键收集超时且没有输入 |
| `audio_complete` | `interrupted: bool` | 播放结束；播放期间收到 DTMF 时 `interrupted=true` |
| `api_response` | `status: u16`、`body: json` | `api` 动作返回；`body` 为原始 JSON 值 |
| `phone_collected` | `number: string` | 电话号码输入收集完成 |
| `recording_complete` | `url: string`、`duration_secs: u64` | 录音/语音留言采集完成 |
| `recording_started` | `segment_type: string`、`segment_id: string` | 通话中 `record_start` 已接受，立即请求下一动作 |
| `recording_stopped` | `reason: string?` | 通话中 `record_stop` 已接受，立即请求下一动作 |
| `input_voice` | `text: string`、`confidence: f32` | ASR 识别结果（置信度 0.0–1.0） |
| `error` | `reason: string` | 执行错误，例如 TTS 播放失败（未配置 TTS 服务且 edge-cli 回退不可用）；见“错误处理” |
| `dtmf_menu_invalid` | `digit: string` | 菜单模式中按键不在 `entries` 内，且未设置 `invalid_action` |
| `dtmf_menu_timeout` | 无 | 菜单模式中超时前没有按键，且未设置 `timeout_action` |

---

## 4. 响应 — ActionNode

每个响应都是包含 `"type"` 字段的 JSON 对象，分为两类：

### 终结动作（执行后退出 IVR）

```json
{ "type": "transfer",       "target": "2001" }
{ "type": "transfer",       "target": "ivr:other_ivr", "params": {"order_id":"123"} }
{ "type": "hangup",         "prompt": null }
{ "type": "queue",          "target": "support" }
{ "type": "voicemail",      "target": "1001" }
{ "type": "play_and_hangup","prompt": "goodbye.wav", "code": 200 }
{ "type": "jump_ivr",       "route_point": "other_ivr", "params": {"key":"val"} }
{ "type": "route_to_agent", "target": "9200", "skill_group_id": "sales" }
{ "type": "voip_bridge",    "create_room_uri": "wss://...", "headers": {...}, "timeout_ms": 30000 }
```

### 非终结动作（执行、等待下一事件，然后再次调用 Provider）

```json
{ "type": "prompt",        "file": "hello.wav",    "interruptible": true }
{ "type": "dtmf_menu",     "greeting": "menu.wav", "entries": {...} }
{ "type": "collect_dtmf",  "min_digits": 1,        "max_digits": 4, "timeout_ms": 5000 }
{ "type": "input_phone",   "prompt": "enter.wav" }
{ "type": "input_voice",   "scene": "order",       "timeout_ms": 8000 }
{ "type": "api",           "url": "https://api.example.com", "method": "POST", "timeout": 10 }
{ "type": "torecord",      "prompt": "leave_msg.wav", "beep": true }
{ "type": "record_start",  "segment_type": "ivr", "id": "seg1", "beep": false }
{ "type": "record_stop" }
```

> 通话中录音：`record_start` / `record_stop` 可随时使用——录音捕获通道对所有通话常备，无需 `[recording].enabled`。分段自动命名为 `{root_session_id}_{seq}_{label}.wav`：`seq` 为本通通话内递增序号，`label` 自动解析为当前坐席 id（坐席接通后）或 IVR 名（IVR 阶段），均无则回退 `segment_type`。片段记录在 CDR 的 `metadata.recording_segments` 中，上传后每个片段各触发一条 `recording_metadata_available`。在 `transfer` / REFER 前调用 `record_stop`，确保当前片段正常结束。`torecord` 仍为语音信箱式采集，会等待 `recording_complete`。

### 透明透传字段

每个 `ActionNode` 响应都可以包含三个可选顶层字段，用于节点标识和数据透传：

```json
{
  "type": "prompt",
  "file": "hello.wav",
  "step_id": "1000602002200750100",
  "step_name": "欢迎语",
  "extra": {
    "tenantId": "didi",
    "gvpFlow": "CTCDaiJiaKeFu",
    "callPath": "F_11,F",
    "businessType": "6",
    "customerType": "1",
    "routePoint": "39325"
  }
}
```

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `step_id` | `string` 或 null | 节点标识，存储并以 `step_id` 在 `ivr_step_trace` 事件中输出 |
| `step_name` | `string` 或 null | 节点名称，存储并以 `step_name` 输出 |
| `extra` | `JSON Object` 或 null | 透明透传数据。Provider 每次返回完整对象；RustPBX 保存它，并在后续每个 `ivr_step_trace` 事件中原样包含 |

> **透传行为**：`session_start` 之后，Provider 应在每次响应中包含 `extra`。RustPBX 保存最新 `extra` 并在所有 `ivr_step_trace` 事件中回显，直到 Provider 更新它。当前步骤的追踪使用最近一次 Provider 响应中的 `step_id` 和 `step_name`。

### Next 串联

非终结动作可以包含 `next` 字段，无需额外往返即可串联多个动作：

```json
{
  "type": "prompt",
  "file": "hello.wav",
  "interruptible": true,
  "next": {
    "type": "dtmf_menu",
    "greeting": "menu.wav",
    "timeout_ms": 5000,
    "entries": {
      "1": { "type": "transfer", "target": "2001" },
      "2": { "type": "queue", "target": "support" }
    }
  }
}
```

RustPBX 播放提示音 → 音频完成 → 自动执行 dtmf_menu（两者之间不会向 Provider 发送 POST）。

---

## 5. 动作类型参考

### 终结动作

| type | 用途 | 必填 | 可选 | 备注 |
|------|---------|----------|----------|-------|
| `transfer` | 将通话盲转到 SIP URI、分机或另一个 IVR | `target: string` | `params: Map<string,string>`、`return_to_ivr: string` | `target` 为 `ivr:` URI 时，`params` 作为会话变量传入。`return_to_ivr`（IVR ID）指定 B 腿挂断后返回哪个 IVR |
| `queue` | 将主叫送入 ACD 队列 | `target: string` | `return_to_ivr: string` | 设置 `return_to_ivr`（IVR ID）后，若无坐席接听或已连接坐席挂断，则返回该 IVR |
| `voicemail` | 将主叫转到用户语音信箱 | `target: string` | — | 通话离开 IVR |
| `hangup` | 终止通话 | — | `prompt: string or null` | 设置 `prompt` 时先播放音频再挂断 |
| `play_and_hangup` | 播放公告后以 SIP 状态码挂断 | — | `prompt: string or null`、`code: u16 or null` | `code` 为 SIP 响应码（例如 486 忙线、404 未找到） |
| `jump_ivr` | 跳到另一个命名 IVR 路由点 | `route_point: string` | `params: Map<string,string>` | 等价于 `target="ivr:{route_point}"` 的 `transfer`。`params` 作为会话变量传给目标 IVR |
| `route_to_agent` | 按技能路由到人工坐席 | `target: string` | `skill_group_id: string`、`key_id: string`、`channel_code: string` | 使用呼叫中心路由引擎 |
| `voip_bridge` | 将通话音频以原始 PCM16 桥接到外部 WebSocket 端点 | `create_room_uri: string` | `headers: Map<string,string>`、`timeout_ms: u64` | 见 [voip_bridge_en.md](voip_bridge_en.md) |

> **在 IVR 之间传递数据**：`transfer`（`target="ivr:other_ivr"`）和 `jump_ivr` 都支持 `params`。这些参数经 URL 编码后作为查询字符串附加到目标 URI（例如 `ivr:other_ivr?order_id=123`），并在接收端解析。目标 IVR 将其合并入会话变量，使其可用于 `$var$` 替换，并包含在发往外部 Step Provider 的 `ProviderContext.variables` 中。这样无需管理外部状态即可在 IVR 间传递上下文。

### 非终结动作

| type | 用途 | 必填 | 可选 | 备注 |
|------|---------|----------|----------|-------|
| `prompt` | 播放音频文件或 TTS | — | `file: string`、`tts_text: string`、`tts_voice: string`、`interruptible: bool`（默认 false）、`record_name_list: string` | `file` 与 `tts_text` 互斥。下一事件：`audio_complete` |
| `dtmf_menu` | 播放欢迎语并等待 DTMF，通过 `entries` 本地解析 | — | `greeting: string`、`greeting_text: string`、`entries: Map<string,ActionNode>`、`timeout_ms: u64`（默认 5000）、`max_retries: u32`（默认 3）、`timeout_action: ActionNode`、`invalid_action: ActionNode` | `entries` 非空时本地处理 DTMF；否则以 `dtmf_menu_invalid`/`dtmf_menu_timeout` 转给 Provider。下一事件：`dtmf`、`dtmf_timeout` 或 entries 匹配到的终结动作 |
| `collect_dtmf` | 收集固定数量 DTMF 数字 | — | `min_digits: usize`（默认 3）、`max_digits: usize`（默认 4）、`timeout_ms: u64`（默认 5000）、`terminator: string`（如 `"#"`）、`prompt: string` | 结果存于会话变量 `dtmf_input`。下一事件：`dtmf`（逐数字）或 `dtmf_timeout` |
| `input_phone` | 收集电话号码（默认 11 位） | — | `prompt: string`、`min_digits: usize`（默认 11）、`max_digits: usize`（默认 11） | 结果存于会话变量 `phone_number`。下一事件：`phone_collected` |
| `input_voice` | ASR 语音输入 | `scene: string` | `timeout_ms: u64`（默认 5000） | ASR 不可用时向 IVR 执行器返回 `WaitFor`，再由执行器向 Provider 发送 `error` 事件。下一事件：`input_voice` |
| `api` | 调用外部 HTTP API | `url: string` | `method: string`（默认 `"GET"`）、`headers: Map<string,string>`、`variables: string`（逗号分隔的待传变量名）、`timeout: u64`（默认 10，单位秒）、`get_dynamic_tree: bool` | 响应体以 `api_response.body` 返回。下一事件：`api_response` |
| `torecord` | 采集录音/语音留言 | — | `prompt: string`、`beep: bool`（默认 false）、`max_duration_secs: u32 or null` | 保存至 `recordings/{session_id}/{timestamp}.wav`。下一事件：`recording_complete` |
| `record_start` | 启动通话中录音片段（不等待） | — | `segment_type` / `type_id: string`（默认 `ivr`）、`id: string`、`beep: bool`、`max_duration_secs: u32` | 无需 `[recording].enabled`。文件：`{root_session_id}_{seq}_{label}.wav`（`label` 自动取坐席 id / IVR 名，回退 `segment_type`）。随后立即请求 Provider（`recording_started`） |
| `record_stop` | 停止当前通话中录音片段 | — | `reason: string` | 建议在 `transfer`/REFER 前使用。随后立即请求 Provider（`recording_stopped`） |

### DtmfMenu 本地解析（Step 模式）

`entries` 非空时，DTMF 在本地解析，无需调用 Provider：

```
User presses a key
  ├── key in entries?       → execute mapped ActionNode immediately
  ├── invalid_action set?   → play it, retry; if retries exhausted, execute invalid_action
  └── no invalid_action?    → forward DTMF to provider as `dtmf_menu_invalid` event
```

设置了 `timeout_action` 时，超时执行该动作；否则向 Provider 发送 `dtmf_menu_timeout`。

### DTMF 事件过滤（提前输入）

RustPBX 会过滤 IVR **未等待用户输入**时到达的 DTMF，防止音频播放期间意外按键打乱流程。

| IVR 状态 | DTMF 行为 |
|-----------|---------------|
| 正在播放不可打断的 `prompt` | **忽略**，静默丢弃数字 |
| 正在播放可打断的 `prompt` | 打断播放，将数字转给 Provider |
| 正在播放 `dtmf_menu` 欢迎语 | 通过 `entries` 本地解析（打断输入） |
| `dtmf_menu` 欢迎语结束（`awaiting_dtmf`） | 本地解析或转给 Provider |
| 正在执行 `collect_dtmf` / `input_phone` | 由数字收集器消费 |
| 正在执行任意终结动作 | **忽略** |

> **注意：** 如果 Provider 希望不受状态限制地接收所有 DTMF 数字，请确保前一个动作是 `dtmf_menu` 或 `collect_dtmf`，而不是普通 `prompt`。推荐使用 `next` 将 `prompt` 与 `collect_dtmf` 串联：先由 `prompt` 播放提示音，音频完成后才由 `collect_dtmf` 开始接收数字。

---

## 6. 构建 Provider

### 状态机模式（Python）

Provider 是按通话维护的状态机。参考实现位于 `examples/unified_ivr_provider.py`：

```python
class IvrSession:
    def __init__(self, caller, callee):
        self.step = "start"
        self.retries = 0

    def next_action(self, event) -> dict:
        ev_type = event.get("type", "")

        # Step 1: initial greeting + DTMF menu (chained in one response)
        if self.step == "start":
            self.step = "menu"
            return {
                "type": "prompt",
                "file": "sounds/ivr/welcome.wav",
                "interruptible": True,
                "next": {
                    "type": "dtmf_menu",
                    "greeting": "sounds/ivr/menu.wav",
                    "timeout_ms": 5000,
                    "entries": {
                        "1": { "type": "transfer", "target": "2001" },
                        "2": { "type": "queue", "target": "support" },
                        "0": { "type": "transfer", "target": "operator" },
                    },
                },
            }

        # Step 2: handle DTMF from the menu
        if self.step == "menu":
            if ev_type == "dtmf":
                digit = event.get("digit", "")
                if digit == "1":  return {"type": "transfer", "target": "2001"}
                if digit == "2":  return {"type": "queue", "target": "support"}
                if digit == "0":  return {"type": "transfer", "target": "operator"}
                self.retries += 1
                if self.retries >= 3:
                    return {"type": "hangup"}
                return {"type": "prompt", "file": "sounds/ivr/invalid.wav",
                        "next": {"type": "repeat"}}
            if ev_type == "dtmf_timeout":
                return {"type": "prompt", "file": "sounds/ivr/timeout.wav",
                        "next": {"type": "repeat"}}

        return {"type": "hangup"}  # fallback
```

### HTTP 服务器骨架

```python
from http.server import HTTPServer, BaseHTTPRequestHandler
import json

class Handler(BaseHTTPRequestHandler):
    sessions = {}

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        sid = body.get("session_id", "")

        if self.path == "/ivr/step/start":
            self.sessions[sid] = IvrSession(body.get("caller"), body.get("callee"))
            self._json(200, {"status": "ok"})

        elif self.path == "/ivr/step/end":
            self.sessions.pop(sid, None)
            self._json(200, {"status": "ok"})

        else:  # /ivr/step (main endpoint)
            session = self.sessions.get(sid)
            if not session:
                session = self.sessions[sid] = IvrSession("unknown", "unknown")
            node = session.next_action(body.get("event", {}))
            self._json(200, node)

    def _json(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())
```

### 三个端点

Provider 提供三个 POST 端点，RustPBX 自动调用它们：

**`POST {url}`**（每一步）——接收 `ProviderContext`，返回 `ActionNode`。

- 请求头：`Content-Type: application/json`，以及配置中的自定义头。
- 请求体：`ProviderContext`（见 §3）。
- 响应体：`ActionNode`（见 §4）。
- 重试：请求失败（超时、5xx、网络错误）时，RustPBX 最多重试 `max_retries` 次。

**`POST {url}/start`**（会话开始）——可选通知，不需要响应。

- 请求体：`SessionContext`（`session_id`、`caller`、`callee`、`direction`、`tenant_id`、`ivr_id`）。
- 发送后不等待，不重试，忽略响应。

**`POST {url}/end`**（会话结束）——IVR 会话因任何原因结束时发送通知，不需要响应。

- 发送后不等待，不重试，忽略响应。
- 请求体为结构化 JSON，包含 `reason`（机器可读标签）与可选 `detail`：

```json
{
  "session_id": "call_abc123",
  "reason": "transfer",
  "detail": "2001"
}
```

#### 结束原因标签

| `reason` | `detail` | 触发时机 |
|----------|----------|----------------|
| `"normal"` | `null` | IVR 完成所有步骤且未转接（例如 `hangup` 动作） |
| `"transfer"` | 目标（例如 `"2001"`） | 通过 `transfer` 转给坐席或分机 |
| `"transfer_to_queue"` | 队列名称（例如 `"support"`） | 通过 `queue` 进入 ACD 队列 |
| `"transfer_to_ivr"` | 路由点（例如 `"main"`） | 通过 `jump_ivr` 跳到另一个 IVR |
| `"hangup"` | `null` | 系统（PBX）主动挂断 |
| `"user_hangup"` | `null` | 用户/远端挂断 |
| `"error"` | 错误消息 | IVR 执行错误（例如重试耗尽后 Provider 仍不可达） |

> 常见场景的**响应示例**：
>
> 用户在菜单期间挂断：
> ```json
> { "session_id": "call_1", "reason": "user_hangup", "detail": null }
> ```
>
> 主叫按 `1` → 转给坐席 `2001`：
> ```json
> { "session_id": "call_1", "reason": "transfer", "detail": "2001" }
> ```
>
> IVR 流程完成，系统挂断：
> ```json
> { "session_id": "call_1", "reason": "normal" }
> ```
> *（`detail` 为 `null` 时，从 JSON 请求体中省略）*

**`POST {url}/dtmf-match`**（可选）——`DtmfMenu` 条目在本地匹配且无需往返便执行动作时调用。发送后不等待，不需要响应。

- 请求体：`{"digit": "1", "action": {"type": "transfer", "target": "2001"}}`。
- 即使没有发送 `ProviderEvent`，也能让 Provider 获知用户输入。

---

## 7. 变量替换

字符串字段中的任意 `$var_name$` 都会被会话变量替换：

```json
{ "type": "transfer", "target": "$agent_extension$" }
{ "type": "api", "url": "https://api.example.com/status/$session_id$" }
```

### 预定义变量

| 变量 | 来源 |
|----------|--------|
| `session_id` | 会话标识 |
| `caller` | 主叫号码 |
| `callee` | 被叫号码 |
| `direction` | 通话方向 |
| `tenant_id` | 租户标识 |
| `dtmf_input` | `collect_dtmf` 的结果 |
| `phone_number` | `input_phone` 的结果 |
| `api_status` | `api` 动作的 HTTP 状态 |
| `api_result` | `api` 动作的响应体 |

Provider 在 `ProviderContext.variables` 中返回的任何变量也都可用。

通过 `transfer`（`target="ivr:other_ivr"`）或 `jump_ivr` 进入 IVR 时，源动作的所有 `params` 会在发送首个 `ProviderEvent::SessionStart` 前合并入会话变量。因此目标 IVR 的 Provider 会立即在 `ProviderContext.variables` 看到这些值，并可在响应中使用 `$param_name$` 替换。

此外，RustPBX 会**自动注入转接来源**：跳转/转接进入的 IVR，其 `ProviderContext.transferred_from` 为 `"ivr"`（或 `"agent"` / `"queue"`），`variables` 中附带 `source_ivr`（来源 IVR 短码）与 `source_node`（来源节点 ID）。Provider 据此可区分"被跳转续接"与"全新进入"，无需业务方手工传参。

---

## 8. 错误处理与重试

| 场景 | 行为 |
|----------|----------|
| **Provider HTTP 超时**（单次请求超时 = `retry.timeout_ms`，默认 1000ms） | 最多重试 `retry.max_retries` 次（默认 3）。重试间隔为 `retry.delay_ms`（默认 100ms） |
| **Provider 返回 5xx** | 与超时相同，进入重试循环 |
| **所有 `/step` 重试耗尽** | 配置了 `[proxy.ivr_fallback]` 则执行会话级 IVR 回退（见下文）；否则执行 `retry.fallback`（默认 `{"type":"hangup","prompt":"sounds/error.wav"}`） |
| **节点执行失败**（转接/桥接启动错误、仅树模式动作等） | 向 `POST {url}/fail` 发送 `fail` 事件，Provider 返回恢复用 `ActionNode`。若 `/fail` 也失败，则执行会话级 IVR 回退 |
| **Provider 返回无效 JSON / 未知动作类型** | 视为 `/step` 失败，按重试耗尽处理 |
| **Provider 返回仅限树模式的动作**（`repeat`/`back`/`play`/`menu`/`collect_extension`/`collect`/`webhook`） | 不执行，进入 `/fail` 路径 |

### `/fail` 端点

通话仍在 Step IVR 会话中而节点失败时，RustPBX 使用与 `/step` 相同的 `ProviderContext` 结构发 POST，其中包含：

```json
{
  "type": "fail",
  "reason": "transfer start failed",
  "failed_step_id": "optional",
  "failed_step_name": "optional",
  "failed_action": "Transfer"
}
```

返回 `ActionNode` 可继续**同一** Provider 会话。不要依赖 `/fail` 恢复 TTS `error`：该情况仍使用 `POST {url}` 携带 `{"type":"error",...}`。

### 会话级 IVR 回退（`[proxy.ivr_fallback]`）

当前 Step Provider 无法继续时使用（`/step` 重试耗尽、`/fail` 失败或目标 IVR 启动失败）。匹配规则与拨号计划路由使用相同的 `from`/`to`/`header.*` 语义；按 `priority` 降序，第一个匹配项生效，否则使用 `default`。

```toml
[proxy.ivr_fallback]
default = "default"

[[proxy.ivr_fallback.rules]]
name = "vip"
priority = 100
match = { "from.user" = "^9", "to.user" = "4000" }
target = "builtin_vip_step"

[[proxy.ivr_fallback.rules]]
priority = 50
match = { "header.X-Tenant" = "acme" }
target = "acme_ivr"
```

RustPBX 通过 `toivr:{target}` 跳转，并设置 `ivr_fallback_used=1`，确保每通通话最多回退一次。再次失败则播放 `sounds/error.wav` 并挂断。

### TTS 音频回退

`prompt` 动作包含 `tts_text`，但没有配置 TTS 服务时：

1. RustPBX 尝试使用 **edge-cli**（Microsoft Edge TTS CLI）作为内置回退进行合成。
2. edge-cli 成功时，正常播放音频。
3. edge-cli 不可用或失败时，RustPBX 向 Provider 发送 `error` 事件（`{"type":"error","reason":"TTS service not available"}`）。Provider 应处理该事件并返回回退动作，例如基于 `file` 的提示音或其他流程。

**重要：** 不要在响应 `error` 事件时再次返回基于 `tts_text` 的提示，否则会重复 TTS 失败，RustPBX 将再次发送 `error` 事件（由于错误 → 回退 → 终结动作链，不会无限循环）。

---

## 9. 配置

### 路由参数（运行时使用）

```json
{
  "mode": "step",
  "url": "http://localhost:8080/ivr/step",
  "headers": {
    "Authorization": "Bearer token123"
  },
  "retry": {
    "max_retries": 5,
    "timeout_ms": 2000,
    "delay_ms": 250,
    "fallback": { "type": "transfer", "target": "operator" }
  },
  "name": "my-step-ivr"
}
```

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `url` | string | — | Provider HTTP 端点（POST）；`/start`、`/end`、`/fail` 从它派生 |
| `headers` | `Map<string,string>` | `{}` | 每次 Provider 调用都会发送的自定义 HTTP 头 |
| `retry.max_retries` | u32 | `3` | 最大重试次数 |
| `retry.timeout_ms` | u64 | `1000` | 单次请求超时，单位为**毫秒** |
| `retry.delay_ms` | u64 | `100` | 失败尝试之间的间隔，单位为**毫秒** |
| `retry.fallback` | ActionNode | `{"type":"hangup","prompt":"sounds/error.wav"}` | `/step` 重试失败**且**未配置 `[proxy.ivr_fallback]` 时，在同一会话中执行的动作 |
| `name` | string | `"step_ivr"` | 用于追踪的显示名称 |

### 发布后的 step.json（由 IVR 编辑器创建，供参考）

```json
{
  "mode": "step",
  "name": "My IVR",
  "url": "https://provider.example.com/ivr/step",
  "headers": { "Authorization": "Bearer token123" },
  "retry": {
    "max_retries": 3,
    "timeout_ms": 1000,
    "delay_ms": 100
  }
}
```

### 路由配置（TOML）

```toml
[[routes]]
name = "my_step_ivr"
to_user = "*99"
app = "ivr"
app_params = { mode = "step", url = "http://localhost:8080/ivr/step" }
```

---

## 10. 测试与调试

### curl

```bash
SESSION="test_$(date +%s)"
# Session start
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"$SESSION\",\"caller\":\"1001\",\"callee\":\"2000\",\"event\":{\"type\":\"session_start\"}}"

# Simulate DTMF
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"$SESSION\",\"event\":{\"type\":\"dtmf\",\"digit\":\"1\"}}"

# Simulate audio complete
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"$SESSION\",\"event\":{\"type\":\"audio_complete\",\"interrupted\":false}}"
```

### 追踪（RustPBX 侧）

每一步都会记录。查看路径：IVR Editor → Debug → 选择会话。每条记录显示实际发送的 `ProviderContext`、返回的 `ActionNode` 和耗时。

RWI 订阅者实时接收追踪条目（事件类型 `ivr_step_trace`）。

### 参考实现

`examples/unified_ivr_provider.py`：完整 Python Provider，无外部依赖。覆盖会话状态机、Prompt → DtmfMenu → Transfer/Queue/Hangup、DTMF 超时处理、无效按键重试三次后挂断，还包括 WebSocket PCM16 桥接回声服务器和 SIP INFO 请求体构造器（`ivr.exec`）。
