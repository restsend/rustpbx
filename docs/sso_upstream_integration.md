# SSO Upstream Integration — Third-Party JWT Handoff (企业 SSO 对接文档 · 上游侧)

RustPBX 内置一个通用 SSO 登录中转（SSO login broker）。本文描述**上游企业
SSO（IdP）需要满足的契约**。客户端侧契约见 `docs/sso_client_integration.md`；端到端接入示例见
`docs/config/sso_walkthrough.md`（[English](config/sso_walkthrough_en.md)）。

> 前置条件：`config.toml` 中 `[sso].enabled = true` 且构建包含 `commerce`
> feature，否则相关端点不存在（404）。

## 总览

```
浏览器                              企业 SSO (你方)                  RustPBX
  │◄─── 302 登录页?state=<sealed> ─────────│
  │──── 账号密码 / 已有会话静默登录 ───────►│
  │                                    │ 签发 JWT (HS256)
  │◄── 302 https://<pbx>/sso/callback?token=<jwt>&state=<原样回传> ───┤
```

## 1. RustPBX 向你方提供

| 项 | 说明 |
|---|---|
| 回调地址 | `https://<pbx-host>{base_path}/callback`，默认前缀 `/sso`（可配 `[sso].base_path`），部署时确定 |
| 共享密钥 | HS256 签名密钥（对应 `[sso.jwt].secret`），线下安全渠道交付；**集群部署时所有节点必须一致** |

## 2. 你方需实现

### 2.1 登录页

URL 由你方提供，配入 `[sso.jwt].upstream_login_url`。

- RustPBX 跳转时会在该 URL 后追加 `&state=<opaque>`（已含 query 则用 `&`）。
- 用户已有你方会话时建议直接 302 回调（静默续登）；否则渲染登录表单。

### 2.2 认证成功后的重定向

```
HTTP/1.1 302 Found
Location: https://<pbx-host>/sso/callback
          ?token=<urlencoded-jwt>
          &state=<RustPBX 下发的 state 原样回传>
```

- `state` 必须逐字节回传，不得裁剪或重编码语义。
- 用户取消/拒绝登录时：302 回调并带 `error=access_denied`（客户端会收到失败深链），或停留你方错误页均可。
- 其余错误（验签失败、state 过期等）由 RustPBX 自行处理，无需你方参与。

### 2.3 JWT 规范（HS256）

```json
{
  "iss": "https://sso.example.com",   // 可选强制校验，配 [sso.jwt].issuer
  "aud": "rustpbx",                   // 可选强制校验，配 [sso.jwt].audience
  "userId": "u12345",                 // 用户唯一 ID，claim 名由 [sso.jwt].user_id_claim 决定
  "exp": 1735689600,                  // 强制校验；建议 TTL ≤ 300s（一次性入场券）
  "iat": 1735689300,
  "email": "user@corp.com"            // 可选：用于本地用户映射与 auto_provision
}
```

- 算法固定 **HS256**，密钥即共享密钥。
- 该 JWT 是一次性入场券，短 TTL 即可；长会话生命周期由客户端凭据体系自行管理。
- 额外 claims（如 `agent_id`、`mis_id`）会原样透传给客户端使用场景。

## 3. 参数名

回调参数名默认为 `token` 与 `state`。如你方固定了其他参数名，请提前告知，
RustPBX 侧可通过代码配置适配（需在部署约定阶段确认）。

## 4. 安全要求

- 回调必须 HTTPS（或同一受信内网）。
- 共享密钥只存两端配置文件，不进日志、不进代码仓库。
- JWT `exp` ≤ 5 分钟。
- `token`/`state` 属敏感参数，RustPBX 访问日志已做脱敏处理；你方日志同理。

## 5. 集群部署（RustPBX 侧说明，供架构评审参考）

RustPBX 采用**无状态信封**设计：授权流程与授权码均为 HMAC 签名的短时效
artifacts，不依赖任何共享存储（无需 Redis/共享 DB）。负载均衡之后
`/authorize`、`/callback`、`/token` 可落在任意节点——你方的回调请求打
到哪个节点都无影响，前提是各节点 `[sso.jwt].secret` 配置完全一致。
