# 路由

> 译文说明：代码、配置及注释原样保留；字段和功能状态按英文原文翻译。

路由决定如何处理呼叫。

## 静态路由（`[[proxy.routes]]`）

在 TOML 中定义。按 `priority` 从高到低求值，再按定义顺序求值。

```toml
[[proxy.routes]]
name = "Internal Calls"
priority = 100
direction = "any" # inbound, outbound, any

# Match Conditions (Regex supported)
[proxy.routes.match]
"to.user" = "^10[0-9]{2}$"
# "from.host" = "internal.net"
# "header.X-My-Header" = "secret"

# Optional Rewrites
[proxy.routes.rewrite]
"to.host" = "127.0.0.1"

# Action fields are flattened into the route:
# action = "forward"
# dest = "trunk-name"
# select = "sequential"
```

### 路由规则字段

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `name` | string | 必填 | 路由名称 |
| `description` | string | `""` | 可选描述 |
| `priority` | int | `0` | 求值顺序（越高越先） |
| `source_trunks` | [string] | `[]` | 仅匹配来自这些中继名称的呼叫 |
| `source_trunk_ids` | [int] | `[]` | 仅匹配来自这些中继 ID 的呼叫 |
| `match` | table | 必填 | 匹配条件（SIP 字段上的正则表达式） |
| `rewrite` | table | 无 | 可选的 SIP 头/用户/主机改写 |
| `codecs` | [string] | `[]` | 限制此路由允许使用的编解码器 |
| `disable_ice_servers` | bool | `false` | 对此路由禁用 ICE |
| `policy` | string | 无 | 计费策略引用 |
| `disabled` | bool | `false` | 禁用路由而不删除它 |
| *平铺的动作字段* | — | — | 见下面的 RouteAction |

### RouteAction 字段

直接平铺在路由内（不嵌套在 `[proxy.routes.action]` 下）。

| 字段 | 类型 | 默认值 | 说明 |
|-------|------|---------|-------------|
| `action` | string | `""` | `forward`、`reject`、`queue`、`app`，或省略以进行本地处理 |
| `dest` | string | 无 | 目标中继或 SIP URI（用于 `forward`） |
| `select` | string | `"rr"` | 目标选择：`"rr"`（轮询）、`"sequential"`、`"parallel"` |
| `hash_key` | string | 无 | 用于目标选择的一致性哈希键 |
| `reject` | int | `403` | `reject` 动作的 SIP 状态码 |
| `queue` | string | 无 | `queue` 动作的队列名称 |
| `app` | string | 无 | 应用名称（例如 `"ivr"`） |
| `app_params` | table | 无 | 应用专属参数（JSON） |
| `auto_answer` | bool | `true` | 路由到应用之前自动接听 |

### 转发示例

将指定前缀转发到中继。

```toml
[[proxy.routes]]
name = "Outbound US"
priority = 10

[proxy.routes.match]
"to.user" = "^1[2-9][0-9]{9}$"

# Route action fields are flattened:
action = "forward"
dest = "provider-trunk" # Name of a defined trunk
select = "sequential"
```

### 队列路由

将呼叫送入队列。

```toml
[[proxy.routes]]
name = "Support Line"

[proxy.routes.match]
"to.user" = "support"

# Route action fields are flattened:
action = "queue"
queue = "support-queue" # Name of queue config
```

## HTTP 动态路由器（`proxy.http_router`）

针对每通呼叫向外部服务请求路由指令。

HTTP Router 的管理和测试位于 **Web Console** 的 **Settings > Proxy Settings**。“Test router”按钮向你的服务发送一个 INVITE 示例载荷，以确认它返回有效的路由 JSON。

```toml
[proxy.http_router]
url = "http://route-engine/decision"
timeout_ms = 500
fallback_to_static = true # If HTTP fails, use static routes
headers = { "X-Api-Key" = "secret" }
```

### 协议详情

**请求载荷（JSON POST）：**

```json
{
  "call_id": "...",
  "from": "sip:alice@...",
  "to": "sip:bob@...",
  "source_addr": "1.2.3.4:5060",
  "direction": "internal",
  "method": "INVITE",
  "uri": "sip:1001@example.com",
  "headers": {
    "User-Agent": "Linphone/...",
    "X-Custom-Info": "..."
  },
  "body": ""
}
```

**响应载荷（JSON）：**

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `action` | string | `forward`、`reject`、`abort`、`not_handled`、`spam` |
| `targets` | [string] | SIP URI 列表（`forward` 时必填） |
| `strategy` | string | `sequential` 或 `parallel`（默认 `sequential`） |
| `status` | int | SIP 状态码（用于 `reject`、`abort`，默认 `403`） |
| `reason` | string | 原因短语（用于 `reject`、`abort`、`spam`） |
| `record` | bool | 是否录制本通通话 |
| `timeout` | int | 最大通话时长，单位为秒（默认 3600） |
| `max_ring_time` | int | 呼叫建立/回铃阶段的最大振铃时长，单位为秒。`0` 或未设置表示禁用振铃超时（一直振铃直到接听或取消）。不对数值进行钳制。覆盖全局 `[proxy] max_ring_time` 以及路由/中继设置 |
| `rtp_timeout` | int | 每个方向的 RTP 超时，单位为秒；任一方向在此时间内未收到音频则终止通话。覆盖代理级 `rtp_timeout`。设为 `0` 禁用（默认使用代理配置，30 秒） |
| `media_proxy` | string | 媒体代理模式：`auto`、`all`、`none`、`nat` |
| `headers` | object | 添加到出站 INVITE 的自定义 SIP 头（键值对） |
| `caller` | string | 覆盖被叫腿的主叫（From 头）。接受完整 SIP URI（`sip:88888888@pbx.example.com`）、`user@host` 简写，或使用主叫 Realm 补全的纯用户名。省略时保留已认证主叫。无法解析的值会使通话以 `500 Server Internal Error` 失败 |
| `with_original_headers` | bool | 按路由覆盖原始头透传策略：`true` 将原 INVITE 的自定义头转发到被叫腿（等价于 `header_passthrough.mode = "all"`）；`false` 不转发。省略时按内部/中继目标决定：内部目标转发所有自定义头，外部中继按 `header_passthrough` 配置转发（默认不转发）。核心 SIP 头（`Via`/`From`/`To`/`Call-ID`/`CSeq`/`Contact`/…）始终不转发 |
| `extensions` | object | 存储在拨号计划中的自定义键值对（可用于 CDR） |

### Python 示例（Flask）

```python
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route("/decision", methods=["POST"])
def route_call():
    data = request.json
    call_to = data.get("to", "")
    
    # Custom business logic: route 1xxx to internal extensions
    if "sip:1" in call_to:
        return jsonify({
            "action": "forward",
            "targets": [call_to.replace("sip:", "sip:ext_")],
            "strategy": "sequential",
            "record": True,
            "media_proxy": "all",  # Force media proxy
            "headers": {
                "X-Custom-Info": "Routed-By-Python"
            },
            "extensions": {
                "account_id": "ACC123",
                "customer_tier": "gold"
            }
        })
    
    # Reject everything else
    return jsonify({
        "action": "reject",
        "status": 403,
        "reason": "Forbidden"
    })

if __name__ == "__main__":
    app.run(port=5000)
```
