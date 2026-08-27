# SSO Walkthrough (SSO 接入演练)

> English version: [sso_walkthrough_en.md](sso_walkthrough_en.md)

从零到拿 token 的完整示例：运维配一次、企业 SSO 方对一次、客户端开发按
时序走一遍。所有命令可直接复制执行。

> 前置：commerce 构建的 rustpbx。相关端点仅在 `[sso].enabled = true` 时存在。
>
> 契约细节见 `docs/sso_upstream_integration.md`（上游侧）与
> `docs/sso_client_integration.md`（客户端侧）。

## 角色

| 角色 | 做什么 |
|---|---|
| 运维（你） | 写 `[sso]` 配置，告知对方回调地址与密钥 |
| 企业 SSO 方 | 提供登录页；登录成功后 302 回 RustPBX 并携带 JWT |
| 客户端开发 | 生成 PKCE → 开浏览器 → 收深链 → 换 token → 带 token 访问 |

---

## 第 0 步：服务端配置（运维）

`config.toml`：

```toml
# SSO 登录中转（必须 commerce 构建）
[sso]
enabled      = true
base_path    = "/sso"
provider     = "jwt"
redirect_url = "myapp://auth/sso"     # 客户端 app 的深链
auto_provision = false                # 首登自动建号（无角色）；需要 /api 直通时开 true

[sso.jwt]
secret             = "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X"   # 与企业 SSO 方线下交付
issuer             = ""              # 留空不校验；能拿到固定 iss 时建议填上
audience           = ""
user_id_claim      = "userId"        # 企业 JWT 里标识用户的 claim 名
upstream_login_url = "https://sso.corp.com/login?app=rustpbx"
token_mode         = "passthrough"   # 客户端拿到的就是企业原版 JWT

# 让 SIP/WS 链路直接接受 SSO token（与上面同一个密钥）
[proxy.jwt_auth]
enabled        = true
secret         = "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X"
user_id_claim  = "userId"
check_local_user = false
```

告知企业 SSO 方两件事：
1. 回调地址 `https://<你的域名>/sso/callback`
2. JWT 规范（HS256、claims）—— 见 `docs/sso_upstream_integration.md`

重启后确认端点已挂载（应返回参数缺失类错误而非 404）：

```bash
curl -si https://pbx.example.com/sso/authorize | head -n1   # 非 404 即挂载成功
```

## 第 1 步：客户端本地准备

```bash
# code_verifier: 43-128 位 URL-safe 随机串
VERIFIER=$(head -c 96 /dev/urandom | openssl base64 | tr '/+' '_-' | tr -d '=\n' | cut -c1-86)
# code_challenge = BASE64URL(SHA256(verifier))
CHALLENGE=$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary \
            | openssl base64 | tr '/+' '_-' | tr -d '=\n')
STATE=$(openssl rand -hex 16)
echo "$VERIFIER" > /tmp/sso_verifier  # 第 2 步兑换时要用
echo "$STATE" > /tmp/sso_state        # 留作防 CSRF 校验
```

打开浏览器（真实场景由 app 调起系统浏览器）：

```
https://pbx.example.com/sso/authorize?code_challenge=<CHALLENGE>&code_challenge_method=S256&state=<STATE>
```

浏览器被重定向到企业 SSO 登录页；登录完成后浏览器尝试跳转
`myapp://auth/sso?code=...&state=...`。

**从深链中取出 `code`，校验 `state`**：

```bash
DEEP_LINK='myapp://auth/sso?code=ab12cd34ef56&state=e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0'
CODE=$(echo "$DEEP_LINK" | sed -E 's/.*[?&]code=([^&]+).*/\1/')
RSTATE=$(echo "$DEEP_LINK" | sed -E 's/.*[?&]state=([^&]+).*/\1/')
[ "$RSTATE" = "$(cat /tmp/sso_state)" ] || { echo "state mismatch!"; exit 1; }
```

## 第 2 步：换 token

```bash
VERIFIER=$(cat /tmp/sso_verifier)
curl -s https://pbx.example.com/sso/token \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode grant_type=authorization_code \
  --data-urlencode "code=$CODE" \
  --data-urlencode "code_verifier=$VERIFIER"
```

响应（passthrough 模式，access_token 就是企业 JWT 原文）：

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
  "token_type": "Bearer",
  "expires_in": 287
}
```

## 第 3 步：使用 token（三条链路）

```bash
TOKEN="eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

**REST API（Bearer）**

```bash
curl -s https://pbx.example.com/api/extensions \
  -H "Authorization: Bearer $TOKEN"
```

**SIP WebSocket**

```
wss://pbx.example.com/ws?token=<TOKEN>
```

**SIP 注册/呼叫（REGISTER 头）**

```sip
REGISTER sip:pbx.example.com SIP/2.0
X-Auth-Token: <TOKEN>
```

过期后（`expires_in` 到期）从第 1 步静默重跑：IdP 会话 cookie 通常有效，
浏览器秒级跳回，用户无感知。

---

## 推荐：用 mock SSO server 跑真实链路

`examples/test_sso_server.py` 完整模拟企业 IdP 契约（登录页、HS256 签发、
state 回传、拒绝路径），纯标准库、无需对方参与：

```bash
python3 examples/test_sso_server.py \
    --port 9000 \
    --secret "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X" \
    --issuer "https://sso.example.com" \
    --callback-url "http://127.0.0.1:8088/sso/callback"

# 对应 rustpbx 配置：
#   [sso.jwt] secret / issuer 同上；
#   upstream_login_url = "http://127.0.0.1:9000/login?app=rustpbx"
# 联调脚本可加 --auto（免表单直接批准）。
```

浏览器打开 `http://<pbx>/sso/authorize?...` 即会被送到 mock 登录页，
提交后自动回到深链——与真实企业 SSO 行为一致。

## 附：无浏览器自测脚本（上游接入前先验证服务端）

以下流程不依赖 mock server，用命令行手工模拟"企业 SSO 已接好"的完整链路：

```bash
#!/usr/bin/env bash
set -euo pipefail
PBX=http://127.0.0.1:8088          # 你的 PBX 地址
SECRET=s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X

# code_verifier / challenge 一对真实匹配值
VERIFIER=$(head -c 96 /dev/urandom | openssl base64 | tr '/+' '_-' | tr -d '=\n' | cut -c1-86)
CHALLENGE=$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary \
            | openssl base64 | tr '/+' '_-' | tr -d '=\n')

# 1. 签一个测试用"企业 JWT"（模拟 IdP 输出）
UPSTREAM_JWT=$(python3 - <<EOF
import base64, hashlib, hmac, json, time
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
h=b64(json.dumps({"alg":"HS256","typ":"JWT"}).encode())
p=b64(json.dumps({"userId":"1001","email":"alice@corp.com",
                  "exp":int(time.time())+300}).encode())
sig=b64(hmac.new(b"$SECRET", f"{h}.{p}".encode(), hashlib.sha256).digest())
print(f"{h}.{p}.{sig}")
EOF
)

# 2. 发起授权（challenge 用上面生成的一对），取回发给上游的 state 信封
LOC=$(curl -si "$PBX/sso/authorize?code_challenge=$CHALLENGE&code_challenge_method=S256&state=demo" \
      | grep -i '^location:' | tr -d '\r' | awk '{print $2}')
FLOW_STATE=$(echo "$LOC" | sed -E 's/.*[?&]state=([^&]+).*/\1/')
echo "flow envelope: ${FLOW_STATE:0:40}..."

# 3. 模拟企业 SSO 登录完成后的回调
LOC2=$(curl -si "$PBX/sso/callback?token=$UPSTREAM_JWT&state=$FLOW_STATE" \
       | grep -i '^location:' | tr -d '\r' | awk '{print $2}')
echo "deep link: $LOC2"

CODE=$(echo "$LOC2" | sed -E 's/.*[?&]code=([^&]+).*/\1/')

# 4. 兑换 token（PKCE 校验通过才发）
curl -s "$PBX/sso/token" \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode grant_type=authorization_code \
  --data-urlencode "code=$CODE" \
  --data-urlencode "code_verifier=$VERIFIER" | jq .
```

### 自测脚本的预期输出

第 2 步打印的信封、第 3 步的深链、第 4 步返回：

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",  // 与 UPSTREAM_JWT 一致
  "token_type": "Bearer",
  "expires_in": 29x
}
```

## 常见问题速查

| 现象 | 原因 |
|---|---|
| 端点 404 | 未启用：`[sso].enabled` 或 commerce 构建 |
| `/authorize` 400 `missing code_challenge` | 少参或空参 |
| `/callback` 400 `invalid or expired state` | 流程信封超时（`flow_ttl_secs`）、被重复使用或密钥不一致 |
| `/token` `invalid_grant` | 授权码超时（`code_ttl_secs`）/ **code_verifier 与 challenge 不匹配** |
| `/api` 401 且日志有 `no local user` | `auto_provision=false` 且企业身份未映射到本地用户 |
