# SSO Client Integration — Native App (客户端对接文档)

RustPBX SSO 登录中转对客户端暴露标准 **OAuth2 授权码 + PKCE** 流程
（RFC 6749 / RFC 7636 / RFC 8252 native app）。所有协议、所有企业上游，
客户端看到的契约完全一致。

> 前置条件：服务端 `[sso].enabled = true`（commerce 构建），否则端点不存在。
> 端到端接入示例见 `docs/config/sso_walkthrough.md`（[English](config/sso_walkthrough_en.md)）。

## 端点

| 端点 | 方法 | 说明 |
|---|---|---|
| `{base_path}/authorize` | GET | 发起授权；默认前缀 `/sso` |
| `{base_path}/token` | POST | 换取 access_token / 刷新 |

`{base_path}/callback` 是**服务端**接收企业 IdP 回调用的，客户端无需关心。

## 完整时序

```
1. 本地生成 code_verifier(43-128 位 URL-safe 随机串)
   code_challenge = BASE64URL(SHA256(code_verifier))

2. 打开系统浏览器:
   GET {base_path}/authorize
       ?code_challenge=<challenge>
       &code_challenge_method=S256
       &state=<客户端随机生成并持久化>

3. 服务端完成企业 SSO 登录后, 浏览器被深链回 app:
   <redirect_url>?code=<授权码>&state=<state>    # 如 myapp://auth/sso
   → 校验 state 与步骤 2 一致, 不一致立即终止
   (redirect_url 由部署方在 [sso].redirect_url 配置, 客户端不自行选择;
    深链已带 query 时, code/state 以 & 追加)

4. 后台换 token (application/x-www-form-urlencoded):
   POST {base_path}/token
   grant_type=authorization_code&code=<授权码>&code_verifier=<verifier>

   200 {"access_token":"...","token_type":"Bearer","expires_in":N}

5. 使用 access_token 访问 RustPBX:
   SIP REGISTER        头: X-Auth-Token: <access_token>
   REST API            头: Authorization: Bearer <access_token>
   SIP WebSocket       wss://<pbx>/ws?token=<access_token>

6. token 过期(passthrough 模式): 从步骤 2 重跑整个流程。
   企业 IdP 会话 cookie 通常仍然有效 → 浏览器静默跳转、秒级完成,
   用户无感知。token_mode=minted 时另有 refresh_token 可静默刷新。
```

## 错误处理

- 授权码错误统一 RFC 6749 格式：`400 {"error":"invalid_grant"}` 等。
- 收到 `invalid_grant`（过期/重放/verifier 不符）：从步骤 2 重跑。
- 用户在 IdP 取消登录：深链携带 `error=access_denied&state=<state>`，按用户取消处理。

## 约束与安全要点

- **授权码单次语义**：PKCE 保证没有 `code_verifier` 的任何一方无法使用
  授权码；请勿把 verifier 写日志或跨设备传递。
- `expires_in` 以秒计，来自企业 IdP 的 JWT `exp`；建议提前 ~60s 触发续期。
- `state` 是 CSRF 防线，必须每次会话随机生成并在收到深链后先比对再继续。

## 集群部署说明

服务端采用**无状态信封**设计：授权流程与授权码均为 HMAC 签名的一次性
artifacts（TTL 内有效），不依赖服务器间共享存储。LB 之后 `/authorize`、
`/callback`、`/token` 可以落在任意节点，前提是各节点配置完全相同的密钥
（`[proxy.jwt_auth].secret` 或 `[sso.jwt].secret`）。

*已知边界*: `token_mode = "minted"` 的 refresh_token 目前为单节点内存态；
passthrough 模式不受影响（推荐集群一律使用 passthrough）。
