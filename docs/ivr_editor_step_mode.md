# IVR Editor 步模式配置与调试

> IVR Editor 支持两种模式：**树模式**（拖拽式菜单编辑）和**步模式**（配置外部 Provider，通过 JSON 协议逐步驱动 IVR 执行）。两种模式统一使用 TOML 配置文件。

---

## 1. 创建步模式项目

1. 进入 IVR Editor → 点击 **New Project**
2. 在弹出的对话框中：
   - **Name**: 填写项目名称（如 `support_ivr`）
   - **Mode**: 选择 `Step (external provider)`
   - **Description**: 可选描述
3. 点击 **Create**

创建后项目默认包含 Provider 配置模板：

```json
{
  "ivr_mode": "step",
  "provider": {
    "url": "",
    "headers": {},
    "max_retries": 3,
    "retry_delay_ms": 1000,
    "timeout_secs": 10
  }
}
```

---

## 2. 编辑 Provider 配置

在项目编辑页面的 **Step Mode** 表单中填写：

### Provider URL

外部模块的 HTTP 端点，rustpbx 在每个 IVR 步骤时向其发送 POST 请求。

### Headers

可选的自定义 HTTP 请求头：

| Key | Value |
|---|---|
| `Authorization` | `Bearer eyJhbGciOiJIUzI1NiIs...` |
| `X-API-Key` | `your-api-key` |

### Retry

Provider 请求失败时的重试配置：

| 参数 | 默认值 | 说明 |
|---|---|---|
| Max Retries | 3 | 最大重试次数 |
| Delay (ms) | 1000 | 重试间隔（毫秒） |
| Timeout (s) | 10 | 每次请求超时时间（秒） |

重试全部失败后自动播 `sounds/error.wav` → 挂断。

---

## 3. 路由配置

1. 进入 **Routing** → 新建或编辑路由规则
2. **Target Type** 选择 **IVR**
3. 在 IVR Project 下拉框中选择刚才创建的步模式项目
4. 保存路由规则

路由层自动根据项目 mode 创建 `StepIvrApp` 实例，并自动注入：
- `IvrTraceCollector` — 步骤追踪
- `TtsService` — TTS 合成（如配置）
- `RwiGateway` — 实时事件推送

---

## 4. 发布

点击 **Publish** 发布步模式项目。发布流程：

1. 从 `current_data` 读取 Provider 配置
2. 生成 `config/ivr/{name}.toml` 配置文件（包含 `mode = "step"` 和 `[ivr.provider]` 段）
3. 更新数据库中的 `published_data` 和版本号
4. 执行路由热更新（reload）

生成的 TOML 文件示例：

```toml
[ivr]
name = "support_ivr"
mode = "step"

[ivr.provider]
url = "https://provider.example.com/ivr/step"
max_retries = 3
retry_delay_ms = 1000
timeout_secs = 10
# Wire format for resumed flows (voip_bridge return with no buffered digits):
#   "resume"        (default) — POST {"type":"resume", ...}; endpoints that
#                     reject it are auto-downgraded to session_start
#   "session_start" — legacy wire format (ivr_status=resuming still marks it)
# resume_event_mode = "resume"

[ivr.provider.headers]
Authorization = "Bearer token123"
```

步模式不需要 TTS 缓存预生成。

---

## 5. 调试

### 入口

IVR Editor 项目列表 → 步模式项目显示 **Debug** 按钮 → 点击进入。

### Debug 页面功能

| 功能 | 说明 |
|---|---|
| **Refresh** | 手动刷新 trace 数据 |
| **Auto-refresh** | 每 3 秒自动刷新（适合实时查看） |
| **Session 列表** | 显示最近 500 个 trace session |
| **Timeline** | 展开后显示完整 ActionNode JSON |
| **Clear trace** | 清除选中 session 的 trace 数据 |
| **→ Test in Diagnostics** | 跳转到诊断页 Web Dialer 测试 |

### Trace 条目

每步记录：

| 字段 | 示例 | 说明 |
|---|---|---|
| `timestamp` | `16:30:01.023` | 执行时间 |
| `trigger` | `{"type":"dtmf","detail":{"digit":"1"}}` | 触发信息（type 为触发源类型，detail 为结构化详情，如 DTMF 的 `{"digit":"1"}`） |
| `action_type` | `Transfer` | 执行的 action 类型 |
| `action_json` | `{"type":"transfer",...}` | 完整的 ActionNode JSON |
| `step_end_time` | `16:30:03.120Z` | 步骤结束时间（完成标记，每条记录都有） |
| `duration_ms` | `42ms` | 耗时 |
| `error` | `timeout` | 错误信息（如有） |

### RWI 实时推送

步模式 IVR 通过 RWI Gateway 实时推送 `IvrStepTrace` 事件。RWI 客户端订阅后可实时接收 trace 更新。

---

## 6. 诊断页测试

在诊断页 (`/diagnostics`) 的 **Web Dialer** 标签页完成 IVR 测试：

### DTMF Keypad

在呼叫建立后，显示 DTMF 拨号键盘（0-9, \*, #），通过 JsSIP `session.sendDTMF()` 发送 DTMF 信号。

### IVR 项目选择

下拉框显示所有已发布的 IVR 项目（从 DB 加载）。选择项目后自动填入测试号码 `*99{project_id_short}` 到目标输入框。

### Trace 面板

可折叠面板，支持：
- 输入 `session_id` 手动追踪
- 留空自动检测当前活跃 session
- 2 秒间隔自动轮询
- 展开显示完整 ActionNode JSON

### 测试流程

1. JsSIP 注册到 WebSocket SIP 服务器
2. 选择 IVR 项目 → 自动填入测试号码
3. 拨打 → 呼叫通过路由进入步模式 IVR
4. Trace 面板显示实时步骤
5. DTMF 键盘与 IVR 交互
6. 挂断 → trace 自动停止

---

## 7. Provider 接口规范

### 请求

```
POST {url} (在项目配置中设置)
Content-Type: application/json

{
  "session_id": "call_abc123",
  "caller": "1001",
  "callee": "2000",
  "direction": "inbound",
  "variables": { "dtmf_input": "123" },
  "event": {
    "type": "dtmf",
    "digit": "1"
  }
}
```

### 响应

```json
{
  "type": "dtmf_menu",
  "greeting": "menu.wav",
  "entries": {
    "1": { "type": "transfer", "target": "2001" },
    "2": { "type": "queue", "target": "support" }
  }
}
```

完整 ActionNode 类型定义见 [ivr_node_protocol.md](ivr_node_protocol.md)。

### 会话管理

Provider 应自行管理 session 状态（通过 `session_id` 键）。可选的生命周期回调：

| 端点 | 时机 |
|---|---|
| `POST {url}/start` | 呼叫进入 IVR（通知） |
| `POST {url}/end` | IVR 会话结束（清理） |

---

## 8. 故障排查

| 现象 | 可能原因 | 排查 |
|---|---|---|
| 呼叫进入 IVR 后无响应 | Provider URL 配置错误 | 检查 Provider URL 和路由配置 |
| Trace 中显示 provider_call 超时 | Provider 响应慢或不可达 | 检查网络和 Provider 日志 |
| Trace 条目 `error` 字段有值 | Provider 返回 HTTP 错误 | 检查 Provider 响应状态码 |
| 路由配置中找不到步模式项目 | 项目未发布 | 先 Publish 再配置路由 |
| Debug 页面显示 Tree 模式提示 | 项目 mode 为 tree | 确认创建时选择了 Step 模式 |
| DTMF 键盘不显示 | 未有活跃呼叫 | 先建立 JsSIP SIP 会话 |
| Trace 面板为空 | 未输入 session_id 或无活跃 session | 留空自动检测或手动输入 `*99{id}` |

---

## 9. Python Provider 示例

`examples/unified_ivr_provider.py` 是一个完整的、零外部依赖的 Python 实现：

```python
# 启动
python3 examples/unified_ivr_provider.py 8080

# 自检 (状态机 + PCM16 bridge echo 验证)
python3 examples/unified_ivr_provider.py --self-test

# 单元测试
python3 -m unittest examples/unified_ivr_provider.py
```

实现了：
- HTTP 服务 (POST /ivr/step, /ivr/step/start, /ivr/step/end)
- Session 状态机管理
- Prompt → DtmfMenu → Transfer/Queue/Hangup 完整流程
- DTMF 超时处理
- 无效按键 3 次后挂断
