# 认证与用户

> 译文说明：本篇完整翻译英文说明，保留全部代码、配置和请求示例（含原注释）。原文的默认值与实现状态未在本次翻译中另行修改。

RustPBX 支持多种后端进行用户认证和查询。可在 `proxy.user_backends` 中串联多个后端。

后端按定义顺序查询。如果第一个后端没有找到用户，则继续查询下一个。

> **管理**：可在 **Web Console** 的 **Settings > Proxy Settings** 中添加、删除和配置这些后端。控制台还提供 **Test** 功能，便于在应用变更前验证后端连通性和响应逻辑。

## 1. 内存后端（静态）

最适合小型静态部署或测试。

```toml
[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
username = "1001"
password = "secret-password"
realm = "example.com"  # Optional
display_name = "Alice"
enabled = true
# Allow calls from this user without Registration (IP-based auth usually handled elsewhere)
allow_guest_calls = false 
```

## 2. 数据库后端

从 SQL 数据库（`database_url`）加载用户。

```toml
[[proxy.user_backends]]
type = "database"
# Optional overrides for table schema
table_name = "users"
id_column = "id"
username_column = "username"
password_column = "password"
realm_column = "realm"
enabled_column = "is_active"
```

## 3. HTTP 后端（远程）

将认证交给外部 Web 服务。

```toml
[[proxy.user_backends]]
type = "http"
url = "http://auth-service/verify"
method = "POST"           # Optional: "GET" (default) or "POST"
# username_field = "user"   # Optional: default "username"
# realm_field = "domain"    # Optional: default "realm"
# headers = { "X-Api-Key" = "secret" }
```

### 协议详情

- **请求（GET）**：`?username=1001&realm=example.com`。
- **请求（POST）**：表单编码的请求体，包含 `username` 和 `realm`。
- **响应（成功）**：必须返回 HTTP 200 OK，JSON 对象表示 `SipUser`。
- **响应（错误）**：HTTP 4xx/5xx，附带 JSON 错误载荷。

#### 成功载荷（`SipUser`）

| 字段 | 类型 | 说明 |
|-------|------|-------------|
| `username` | string | SIP 用户名 |
| `password` | string | SIP 密码 |
| `realm` | string | SIP Realm（可选） |
| `display_name` | string | 主叫显示名称（可选） |
| `enabled` | bool | 用户是否启用 |
| `allow_guest_calls` | bool | 是否允许不注册直接呼叫 |

#### 错误载荷

认证失败时，服务器应返回非 2xx 状态码及以下 JSON：

```json
{
  "reason": "invalid_credentials",
  "message": "Optional detailed message"
}
```

**已知原因：**

- `not_found`、`not_user`：用户不存在。
- `invalid_password`、`invalid_credentials`：基本认证失败。
- `disabled`、`blocked`：账号已锁定或禁用。
- `spam`、`spam_detected`：账号被标记为垃圾呼叫来源。
- `payment_required`：余额不足。

### Python 示例（FastAPI）

```python
from fastapi import FastAPI, Form, HTTPException
from typing import Optional, List

app = FastAPI()

@app.post("/verify")
def verify_user(username: str = Form(...), realm: str = Form(...)):
    # Database lookup or logic here
    if username == "1001":
        return {
            "id": 1,
            "enabled": True,
            "username": "1001",
            "password": "secret-password",
            "realm": realm,
            "display_name": "Alice Cooper",
            "allow_guest_calls": False
        }
    
    raise HTTPException(status_code=404, detail="User not found")
```

## 4. 纯文本后端

从简单文本文件加载用户。

```toml
[[proxy.user_backends]]
type = "plain"
path = "./users.txt"
```

## 5. Extension 后端

用于短期、动态分机（通常为内部用途）。

```toml
[[proxy.user_backends]]
type = "extension"
# Optional: separate database for extensions (defaults to main database_url)
# database_url = "sqlite://extensions.sqlite3"
ttl = 3600 # Cache time in seconds
```

---

## HTTP Token 认证后端（一次往返注册）

HTTP 后端可配置为**一次往返 Token 认证**：SIP 客户端在可配置头（例如 `X-Auth-Token`）中携带 Token，由外部 HTTP 服务立即认证，**无需标准 Digest 401/407 挑战往返**。

这类似 JWT 快速注册，但把 Token 校验委托给**外部 HTTP 服务**，而不是在本地校验。同一 HTTP 端点同时服务两条路径：

```
REGISTER with X-Auth-Token header
      │
      ▼
┌─ AuthBackend chain (before 401) ──────────────────────┐
│ HttpTokenAuthBackend:                                 │
│   1. Check SIP request for token_header               │
│   2. Token present → call HTTP service with token     │
│      ├ 200 + SipUser → authenticated (no 407)         │
│      └ non-200 → fall through to Digest               │
│   3. No token → fall through to Digest                │
└───────────────────────────────────────────────────────┘
      │ (token invalid or absent)
      ▼
┌─ Digest 401/407 flow (existing) ──────────────────────┐
│ UserBackend::get_user() → same HTTP URL               │
│ External service returns SipUser with password        │
│ → Digest verification → registration success          │
└───────────────────────────────────────────────────────┘
```

### 配置

为任意 HTTP 用户后端添加 `token_header`，即可启用一次往返 Token 认证：

```toml
[[proxy.user_backends]]
type = "http"
url = "https://auth-service.example.com/sip-auth"
method = "POST"
sip_headers = ["X-Auth-Token"]     # Forward token to external service
token_header = "X-Auth-Token"      # Enable one-shot token auth

# Optional: HTTP client tuning
# http_timeout_ms = 5000            # Default: 5000
# http_retry_count = 1             # Default: 1 (retries on network error + 5xx)
# http_retry_delay_ms = 500        # Default: 500

# Optional: Token cache (avoids repeated HTTP calls for the same token)
# token_cache_ttl_secs = 60        # Default: 0 (disabled). Recommended: 30-300
# token_cache_size = 10000         # Default: 10000 (LRU eviction, prevents memory leak)
```

### 配置字段

| 字段 | 默认值 | 说明 |
|-------|---------|-------------|
| `token_header` | 无 | 携带认证 Token 的 SIP 头名称。设置后启用一次往返 Token 认证 |
| `http_timeout_ms` | `5000` | HTTP 请求超时，单位为毫秒 |
| `http_retry_count` | `1` | 网络错误和 5xx 响应时的重试次数 |
| `http_retry_delay_ms` | `500` | 重试间隔 |
| `token_cache_ttl_secs` | `0` | Token 缓存 TTL，单位为秒。`0` 禁用缓存。推荐 30–300 |
| `token_cache_size` | `10000` | 最大 Token 缓存条目数。使用 LRU 淘汰，防止内存无限增长 |

### Token 缓存与内存安全

`token_cache_ttl_secs > 0` 时缓存成功的 Token 校验结果：

- **键**：Token 的 SHA-256 哈希（不明文存储）。
- **值**：认证后的 `SipUser` + 插入时间戳。
- **淘汰**：LRU（最近最少使用）；缓存满时淘汰最旧条目。
- **过期**：条目在 `token_cache_ttl_secs` 后过期，在下次访问时惰性移除。

这能防止内存泄漏：缓存大小受 `token_cache_size` 限制，条目按 TTL 自动过期。

### 外部服务协议

Token 认证和 Digest 密码查询使用**相同请求格式**。是否存在 Token 字段用于区分两者：

**Token 认证请求（POST）：**

```
POST /sip-auth
Content-Type: application/x-www-form-urlencoded

username=1001&realm=example.com&request_uri=sip:example.com&X-Auth-Token=abc123
```

**Digest 密码查询（POST）**——同一端点，不带 Token：

```
POST /sip-auth
Content-Type: application/x-www-form-urlencoded

username=1001&realm=example.com&request_uri=sip:example.com
```

外部服务可以根据是否存在 Token 实现相应逻辑。

**成功响应**：HTTP 200 + SipUser JSON（格式与现有 HTTP 后端相同）。

**错误响应**：HTTP 4xx/5xx + JSON 错误载荷（格式与现有 HTTP 后端相同）。

### 外部服务示例（FastAPI）

```python
from fastapi import FastAPI, Form, HTTPException

app = FastAPI()

VALID_TOKENS = {
    "abc123": {"username": "1001", "display_name": "Alice"},
    "def456": {"username": "1002", "display_name": "Bob"},
}

@app.post("/sip-auth")
def sip_auth(
    username: str = Form(...),
    realm: str = Form(...),
    request_uri: str = Form(""),
    # Token forwarded via sip_headers config; absent during Digest password lookup
    token: str = Form(None, alias="X-Auth-Token"),
):
    if token:
        # One-shot token auth (no 407)
        token_data = VALID_TOKENS.get(token)
        if token_data and token_data["username"] == username:
            return {
                "id": 1,
                "enabled": True,
                "username": username,
                "realm": realm,
                "display_name": token_data["display_name"],
                # No password needed — token is already validated
            }
        raise HTTPException(403, {"reason": "invalid_credentials", "message": "bad token"})
    else:
        # Digest password lookup (after 401 challenge)
        return {
            "id": 1,
            "enabled": True,
            "username": username,
            "realm": realm,
            "password": "secret-hash",  # Needed for Digest verification
        }
```

### 认证后端链顺序

同时配置 JWT 和 HTTP Token 认证时：

1. **JwtAuthBackend**——本地 JWT 校验（最快）。
2. **HttpTokenAuthBackend**——远程 HTTP Token 校验。
3. 其他自定义 AuthBackend。
4. WS 预认证注册表（WebSocket 连接）。
5. Digest 401/407 回退。

---

## JWT 快速注册（无 401/407 挑战）

RustPBX 支持基于 JWT（HS256）的**快速注册**：SIP 客户端在 REGISTER 请求中携带有效 JWT 时立即通过认证，无需标准 Digest 401/407 挑战往返。

适用于从外部认证服务获取 JWT、希望一次往返完成注册的 WebRTC 软电话（例如 cc-phone SDK）。

### 工作原理

```
External Auth Service issues JWT → SDK connects and registers with JWT
  ↓
AuthModule checks auth_backend chain:
  1. JwtAuthBackend (validates JWT signature + exp + claims)
  2. ... other backends ...
  3. Digest 401/407 fallback (if no JWT or JWT invalid)
```

JWT 有效时，请求以**零次挑战往返**通过。JWT 缺失或无效时，回退到标准 Digest 认证。**现有客户端不受影响。**

### 两条认证路径

| 路径 | 机制 | 传输协议 | 用途 |
|------|-----------|-----------|----------|
| **路径 A**（SIP 头） | REGISTER 中的 `X-Auth-Token: <jwt>` | 全部（UDP/TCP/TLS/WS） | 通用，与传输协议无关 |
| **路径 B**（WS 预认证） | WebSocket 升级时的 `?token=<jwt>` 或 `Authorization: Bearer <jwt>` | 仅 WS/WSS | WebRTC 客户端，连 SIP 层的头都可省去 |

两条路径可以同时使用。配置了 JWT 认证且客户端通过 WebSocket 连接时，路径 B 自动启用。

### 配置

```toml
[proxy.jwt_auth]
enabled = true
secret = "your-shared-hs256-secret"    # HS256 signing secret (shared with JWT issuer)
user_id_claim = "userId"               # JWT claim that maps to SIP username/extension
# issuer = "my-platform"               # Optional: validate iss claim
# audience = "rustpbx-sip"             # Optional: validate aud claim
# sip_header_name = "X-Auth-Token"     # Default: "X-Auth-Token" (path A)
# ws_token_param = "token"             # Default: "token" (path B query param name)
# check_local_user = false             # Default: false. If true, look up user in user_backend chain
```

### 配置字段

| 字段 | 默认值 | 说明 |
|-------|---------|-------------|
| `enabled` | `false` | 启用/禁用 JWT 认证后端 |
| `secret` | 必填 | HS256 共享密钥，必须与 JWT 签发方的签名密钥一致 |
| `user_id_claim` | `"userId"` | 映射到 SIP 用户名的 JWT claim 名称，支持字符串和数字值 |
| `issuer` | 无 | 期望的 `iss` claim 值。设置后拒绝 `iss` 不匹配的 JWT |
| `audience` | 无 | 期望的 `aud` claim 值 |
| `sip_header_name` | `"X-Auth-Token"` | 路径 A 中携带 JWT 的 SIP 头名称 |
| `ws_token_param` | `"token"` | WebSocket URL 中 JWT 查询参数名（路径 B）。也检查 `Authorization: Bearer` 头 |
| `check_local_user` | `false` | 为 `true` 时，JWT 校验后在 `user_backend` 链中查询用户（检查 `enabled`，加载 `display_name`、呼叫转移等）。为 `false` 时仅根据 JWT claims 创建最小 `SipUser` |

### check_local_user：true 与 false

| 对比项 | `false`（默认） | `true` |
|---|---|---|
| 是否需要 HTTP 后端 | 否 | 否（使用本地数据库/缓存） |
| 用户存在性检查 | 无 | 验证 user_backend 中存在用户 |
| `enabled` 检查 | JWT 有效即视为用户启用 | 检查数据库中的 `login_disabled` |
| `display_name` | 来自 JWT 的 `name` claim | 来自数据库 |
| 呼叫转移/语音信箱 | 不可用 | 从数据库获取 |
| 性能 | 最快（零 I/O） | 一次本地查询（带 LRU 缓存） |

**建议**：生产环境使用 `check_local_user = true`，确保被禁用的分机即使持有有效 JWT 也无法注册。

### JWT 格式

JWT 必须使用 **HS256** 算法。必需/可选 claims：

| Claim | 是否必需 | 说明 |
|-------|----------|-------------|
| `<user_id_claim>` | 是 | 映射到 SIP 用户名（例如 `"userId": "1001"`） |
| `exp` | 推荐 | 过期时间戳（Unix 秒）。缺失时 Token 永不过期 |
| `iss` | 配置时必需 | 签发者，必须匹配 `issuer` 配置 |
| `aud` | 配置时必需 | 受众，必须匹配 `audience` 配置 |
| `name` | 可选 | 显示名称（`check_local_user = false` 时使用） |

### JWT 签发示例

**Python：**

```python
import jwt, time
token = jwt.encode(
    {"userId": "1001", "name": "Alice", "exp": int(time.time()) + 3600},
    "your-shared-hs256-secret",
    algorithm="HS256"
)
```

**Node.js：**

```javascript
const jwt = require('jsonwebtoken');
const token = jwt.sign(
    { userId: '1001', name: 'Alice' },
    'your-shared-hs256-secret',
    { expiresIn: '1h' }
);
```

### 客户端 SDK 用法（cc-phone）

```typescript
CCPhone.create({
  server: 'wss://pbx.example.com/ws',
  agentId: '1001',
  jwt: '<your-jwt-token>',      // Fast registration via JWT
  // password: '...',           // Optional: only needed for Digest fallback
})
```

提供 `jwt` 时，SDK 会：

1. 在 WebSocket URL 后附加 `?token=<jwt>`（路径 B 预认证）。
2. 在 SIP REGISTER 中添加 `X-Auth-Token: <jwt>` 头（路径 A）。

### 安全注意事项

1. **密钥管理**：妥善保护 HS256 密钥。使用环境变量或密钥管理服务，不要提交到源码版本控制。
2. **Token 过期**：始终设置 `exp` 限制 Token 生命周期。推荐短期 Token（例如 1 小时）。
3. **回退安全性**：无效或过期 JWT 被静默忽略，请求回退到 Digest 认证，以确保向后兼容。
4. **WS 预认证清理**：在内存注册表中跟踪预认证 WebSocket 连接；连接关闭时清理条目。

---

## Locator（注册存储）

配置“用户 X 在哪里？”这类数据的存储位置。

### 内存（默认）

快速，但重启后丢失。

```toml
[proxy.locator]
type = "memory"
```

### 数据库

持久化注册信息。

```toml
[proxy.locator]
type = "database"
url = "sqlite://rustpbx.sqlite3" # Can share main DB
```

### HTTP（远程）

查询外部注册表。

```toml
[proxy.locator]
type = "http"
url = "http://registry-service/lookup"
```

## Locator Webhook

用户注册状态变化时触发通知。

可直接在 **Web Console** 的 **Settings > Proxy Settings** 中配置并测试此 Webhook。“Test webhook”向端点发送模拟注册事件，以验证连通性和自定义头。

```toml
[proxy.locator_webhook]
url = "http://your-app/sip-events"
events = ["registered", "unregistered", "offline"]
timeout_ms = 5000
headers = { "X-API-Key" = "my-secret-key", "Authorization" = "Bearer token123" }
```

### 事件载荷

Webhook 发送带 JSON 请求体的 POST 请求：

```json
{
  "event": "registered",
  "location": {
    "aor": "sip:1001@example.com",
    "expires": 3600,
    "destination": "udp:192.168.1.100:5060",
    "supports_webrtc": false,
    "transport": "Udp",
    "user_agent": "Zoiper 5"
  },
  "timestamp": 1704537600
}
```

- `event`："registered"、"unregistered" 或 "offline"。
- `location`：SIP 位置信息（仅 `registered` 和 `unregistered`）。
- `locations`：位置数组（仅 `offline`）。
- `timestamp`：Unix 时间戳，单位为秒。

## SSO 登录代理（企业 SSO → 原生应用）

使用标准 OAuth2 授权码 + PKCE 流程，将企业 SSO 登录转换为原生应用深度链接（仅 commerce 构建；`[sso].enabled = true` 时挂载端点）。

- 接口约定：`docs/sso_upstream_integration.md`（上游 IdP）/ `docs/sso_client_integration.md`（客户端应用）。
- 包含可复制命令的端到端示例：[sso_walkthrough.md](sso_walkthrough.md) · [英文](sso_walkthrough_en.md)。

与上述 JWT 快速注册的关键配合方式：让 `[sso.jwt]` 和 `[proxy.jwt_auth]` 共享一个 HS256 密钥，使 SSO 代理签发/转交的 Token 无需修改即可被 SIP（`X-Auth-Token`）与 WebSocket（`?token=`）认证链接受。设计上无状态；节点密钥相同时可安全用于集群，无需共享存储。
