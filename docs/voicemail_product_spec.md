# RustPBX Voicemail Pro 产品方案

**版本**：v1.0  
**日期**：2026-01-26  
**状态**：设计阶段

---

## 📋 产品概述

### 产品定位
企业级智能语音信箱系统，为 RustPBX 用户提供专业的留言管理、智能转写、多渠道通知等功能。

### 目标用户
- **小型企业**（<50人）：需要基础语音留言功能
- **中型企业**（50-200人）：需要可视化留言、邮件集成
- **大型企业**（200+人）：需要配额管理、批量操作、审计

### 核心价值主张
1. **零错过**：所有未接来电自动转语音信箱
2. **智能转写**：语音自动转文字，支持搜索和摘要
3. **多渠道通知**：邮件、短信、Web、移动端实时通知
4. **企业级管理**：配额控制、权限管理、审计日志

---

## 🎯 核心功能

### 1. 基础留言功能

#### 1.1 录制留言
```
用户拨打分机 1001（无人应答 30 秒）
  ↓
自动转接到语音信箱
  ↓
播放欢迎语："您好，您拨打的用户暂时无法接听，请在提示音后留言"
  ↓
提示音（嘟）
  ↓
开始录音（最长 5 分钟）
  ↓
用户按 # 结束录音（或自动结束）
  ↓
"您的留言已保存，再见"
```

**配置参数**：
- 最大录音时长：30秒 ~ 10分钟（默认 5 分钟）
- 欢迎语：系统默认 / 用户自定义
- 结束键：# 或 * （可配置）
- 无声检测：3秒无声自动结束

#### 1.2 查询留言
```
用户拨打 *97（语音信箱快捷码）
  ↓
验证身份（分机密码 / PIN）
  ↓
播放留言数量："您有 3 条新留言"
  ↓
播放留言：
  - 留言 1：来自 13800138000，今天上午 10:23
  - [播放留言内容]
  - 操作提示：
    * 按 1 重播
    * 按 2 保存
    * 按 3 删除
    * 按 4 回拨
    * 按 # 下一条
```

**功能清单**：
- [x] 播放新留言
- [x] 播放已读留言
- [x] 删除留言
- [x] 保存留言（永久保留）
- [x] 回拨功能（自动拨打留言者号码）
- [x] 转发留言（转发给其他分机）

---

### 2. 智能转写（AI 增强）

#### 2.1 语音转文字
- **引擎**：SenseVoice（已集成）
- **语言**：中文、英文、中英混合
- **准确率**：>95%（标准普通话）
- **速度**：实时转写（<1秒延迟）

#### 2.2 智能摘要
```python
原始留言（45秒）：
"你好啊，我是张三，今天下午想跟你聊一下那个项目的事情，
大概三点到四点之间有空吗？如果方便的话给我回个电话，
我的手机号是 138-0013-8000，谢谢。"

AI 摘要：
📞 来电人：张三
⏰ 时间：今天下午 3-4 点
📋 事由：项目讨论
📱 回电：138-0013-8000
```

#### 2.3 关键词提取
- 自动提取：人名、电话号码、时间、地点
- 情绪分析：紧急、普通、不满
- 意图识别：咨询、投诉、预约、推销

---

### 3. 多渠道通知

#### 3.1 邮件通知
```
主题：[语音留言] 来自 13800138000 的新留言

正文：
您好，

您有一条来自 13800138000 的新留言：

来电时间：2026-01-26 10:23:15
留言时长：42 秒
留言内容（转写）：
"你好啊，我是张三，今天下午想跟你聊一下那个项目的事情..."

附件：
- voicemail_20260126_102315.wav（音频文件）
- transcript.txt（转写文本）

在线管理：https://pbx.example.com/voicemail

---
RustPBX Voicemail Pro
```

**配置项**：
- 邮件模板自定义
- 附件格式：WAV / MP3 / Opus
- 发送时机：即时 / 批量（每小时汇总）
- 语言：中文 / 英文

#### 3.2 SMS 短信通知
```
【语音留言】您有1条新留言，来自13800138000，
时长42秒。点击查看：https://pbx.example.com/vm/abc123
```

**集成方案**：
- 国际：Twilio
- 国内：阿里云短信、腾讯云短信

#### 3.3 MWI（消息等待指示器）
```
SIP 话机显示：
┌─────────────────┐
│  💌 3 Messages  │  ← 红灯闪烁
│  1001           │
└─────────────────┘
```

**实现**：
- SIP NOTIFY 消息
- 符合 RFC 3842（Message Summary Event）
- 支持主流 SIP 话机（Yealink、Cisco、Grandstream）

#### 3.4 Web 实时推送
```javascript
// WebSocket 实时通知
{
  "type": "NEW_VOICEMAIL",
  "extension": "1001",
  "caller": "13800138000",
  "duration": 42,
  "timestamp": "2026-01-26T10:23:15Z",
  "transcript": "你好啊，我是张三...",
  "audio_url": "/api/voicemail/abc123/audio"
}
```

---

### 4. Web 管理界面

#### 4.0 分机身份认证

> **架构说明**：Voicemail Pro **复用全局 Enterprise Auth 插件**提供的认证服务。  
> Enterprise Auth 是独立的商业插件（$299-$799/年），提供统一认证层供所有模块使用。

**访问入口**：
- URL: `https://pbx.example.com/voicemail/login`
- 多租户支持：`https://tenant1.pbx.example.com/voicemail`

**认证方式**（由 Enterprise Auth 插件提供）：

```rust
// 复用全局认证服务
use rustpbx_auth::{AuthService, AuthMethod, User};

// Voicemail 只需调用统一认证接口
pub struct VoicemailWebService {
    auth: Arc<AuthService>,  // 注入全局认证服务
}

impl VoicemailWebService {
    async fn login(&self, method: AuthMethod) -> Result<SessionToken> {
        // 调用全局认证服务
        let user = self.auth.authenticate(method).await?;
        
        // 生成 Voicemail 专用 session token
        let token = self.generate_voicemail_token(&user)?;
        
        Ok(token)
    }
}

// 全局认证服务支持的方式（由 Enterprise Auth 插件实现）
pub enum AuthMethod {
    // 方式1：分机号 + PIN码（内置，无需额外插件）
    ExtensionPin {
        extension: String,      // 分机号 1001
        pin: String,            // 4-6 位数字 PIN
        tenant_domain: String,  // 租户域名
    },
    
    // 方式2：SIP凭据（内置，复用现有认证）
    SipCredentials {
        extension: String,
        sip_password: String,
        tenant_domain: String,
    },
    
    // 方式3：LDAP/AD（需要 Enterprise Auth 插件 - 基础版）
    Ldap {
        username: String,
        password: String,
        tenant_domain: String,
    },
    
    // 方式4：SAML SSO（需要 Enterprise Auth 插件 - 企业版）
    SamlToken {
        saml_response: String,
        relay_state: Option<String>,
    },
    
    // 方式5：OAuth/OIDC（需要 Enterprise Auth 插件 - 企业版）
    OAuthCode {
        code: String,
        provider: OAuthProvider,  // Azure, Google, Okta
    },
}

// Voicemail 认证流程（简化）
impl VoicemailWebService {
    async fn login_handler(
        auth: Data<AuthService>,  // 注入全局认证服务
        form: Form<LoginForm>,
    ) -> Result<HttpResponse> {
        // 1. 调用全局认证（自动支持所有认证方式）
        let user = auth.authenticate(form.into_inner().to_auth_method()).await?;
        
        // 2. 生成 Voicemail session token
        let token = generate_voicemail_token(&user)?;
        
        // 3. 返回
        Ok(HttpResponse::Ok().json(json!({
            "token": token,
            "extension": user.extension,
            "expires_at": Utc::now() + Duration::hours(24),
        })))
    }
    
    // 验证token（中间件）
    async fn verify_token_middleware(
        auth: Data<AuthService>,
        req: Request,
    ) -> Result<User> {
        let token = extract_token(&req)?;
        
        // 调用全局认证服务验证
        let user = auth.verify_token(&token).await?;
        
        Ok(user)
    }
}

// 数据库schema（简化 - 不需要存储密码）
CREATE TABLE users (
    id BIGSERIAL PRIMARY KEY,
    tenant_id BIGINT NOT NULL,
    extension VARCHAR(20) NOT NULL,
    display_name VARCHAR(100),
    email VARCHAR(255),
    
    -- 认证方式标记（由 Enterprise Auth 管理）
    auth_method VARCHAR(20) DEFAULT 'pin',  -- pin, ldap, saml, oauth
    
    -- Voicemail PIN码（备用认证，内置支持）
    voicemail_pin VARCHAR(64),  -- bcrypt hash（可选）
    pin_updated_at TIMESTAMPTZ,
    
    -- 用户状态
    status VARCHAR(20) DEFAULT 'active',
    last_login_at TIMESTAMPTZ,
    
    UNIQUE(tenant_id, extension)
);

-- 注意：LDAP/SAML/OAuth 的用户信息由 Enterprise Auth 插件管理
-- Voicemail 只需要基本的 user 信息
CREATE TABLE voicemail_access_logs (
    id BIGSERIAL PRIMARY KEY,
    tenant_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    action VARCHAR(50) NOT NULL,  -- login, view_message, delete_message
    ip_address INET,
    user_agent TEXT,
    status VARCHAR(20) NOT NULL,  -- success, failed
    created_at TIMESTAMPTZ DEFAULT NOW(),
    
    INDEX idx_user_time (user_id, created_at),
    INDEX idx_tenant_time (tenant_id, created_at)
);
```

**安全机制**：

1. **权限隔离**：
```rust
// 中间件：确保用户只能访问自己的留言
pub async fn voicemail_auth_middleware(
    auth: Data<AuthService>,  // 全局认证服务
    req: Request,
    next: Next,
) -> Result<Response> {
    // 1. 解析并验证token（调用全局认证）
    let token = extract_token(&req)?;
    let user = auth.verify_token(&token).await?;
    
    // 2. 注入用户上下文
    req.extensions_mut().insert(user);
    
    // 3. 继续处理
    next.run(req).await
}

// API Handler：自动过滤
async fn list_messages(
    auth_user: Extension<User>,  // 自动注入
) -> Result<Json<Vec<Message>>> {
    // SQL自动加上 user_id 过滤
    let messages = sqlx::query_as!(
        Message,
        r#"
        SELECT * FROM voicemail_messages 
        WHERE user_id = $1 AND deleted_at IS NULL
        ORDER BY created_at DESC
        "#,
        auth_user.id  // 只能查自己的
    ).fetch_all(&db).await?;
    
    Ok(Json(messages))
}

// 下载留言音频
async fn download_message(
    auth_user: Extension<User>,
    Path(message_id): Path<i64>,
) -> Result<impl IntoResponse> {
    // 1. 验证留言属于当前用户
    let message = sqlx::query_as!(
        Message,
        "SELECT * FROM voicemail_messages WHERE id = $1",
        message_id
    ).fetch_one(&db).await?;
    
    if message.user_id != auth_user.id {
        return Err(Error::Forbidden("Not your message"));
    }
    
    // 2. 返回音频文件
    let audio = storage.get(&message.file_path).await?;
    Ok((
        StatusCode::OK,
        [("Content-Type", "audio/wav")],
        audio
    ))
}
```

2. **PIN码管理**（备用认证方式）：
```
初次设置：
- 用户首次登录时可选设置PIN码
- 4-6位数字，不能是顺序号（1234）或重复号（1111）
- bcrypt加密存储

修改PIN：
- 需要验证旧PIN
- 30天内不能重复使用

重置PIN：
- 管理员可重置（需审计日志）
- 通过邮箱验证码自助重置

注意：如果启用了 Enterprise Auth 插件的 LDAP/SSO，PIN码可作为备用认证方式
```

3. **防暴力破解**（由 Enterprise Auth 插件提供）：
```rust
// 全局认证服务已内置防护
// - 登录失败5次锁定15分钟
// - 每IP每分钟最多10次尝试
// - 异常行为自动告警
```

4. **会话管理**：
```
- JWT token有效期：24小时
- 支持多设备同时登录（每个设备独立token）
- 主动登出：清除本地token
- 管理员可强制踢出用户
```

**登录界面**：
```
┌─────────────────────────────────────┐
│   🏢 RustPBX 语音信箱               │
├─────────────────────────────────────┤
│                                     │
│   分机号：[1001        ]            │
│   PIN码 ：[••••        ]            │
│                                     │
│   租户域：[tenant1.pbx.example.com] │
│                                     │
│   [ 登录 ]  [忘记PIN?]              │
│                                     │
│   ────────── 或 ──────────          │
│                                     │
│   [ 使用企业账号登录 ]              │
│   (需要 Enterprise Auth 插件)       │
│                                     │
└─────────────────────────────────────┘
```

---

### 4.0.1 企业LDAP/SSO集成方案

#### 一、LDAP集成

**1. 功能需求**：
- 用户身份验证（替代PIN码）
- 用户信息同步（姓名、邮箱、部门）
- 组织架构映射（部门 → 用户组）
- 支持多LDAP服务器（Active Directory、OpenLDAP）

**2. 依赖库**：
```toml
[dependencies]
ldap3 = "0.11"          # LDAP客户端
tokio-ldap = "0.4"      # 异步LDAP
```

**3. 配置文件**（`config.toml`）：
```toml
[ldap]
enabled = true
server = "ldap://ad.company.com:389"
# 或 LDAPS: "ldaps://ad.company.com:636"

# 绑定账号（用于搜索用户）
bind_dn = "CN=svcacct,OU=ServiceAccounts,DC=company,DC=com"
bind_password = "secret123"

# 搜索基础
base_dn = "OU=Users,DC=company,DC=com"
user_filter = "(&(objectClass=person)(sAMAccountName={username}))"

# 属性映射
attr_username = "sAMAccountName"     # 用户名 → extension
attr_email = "mail"                  # 邮箱
attr_display_name = "displayName"    # 姓名
attr_phone = "telephoneNumber"       # 电话
attr_department = "department"       # 部门

# 连接池
pool_size = 10
timeout_secs = 30

# 用户同步
sync_enabled = true
sync_interval_hours = 6   # 每6小时同步一次
```

**4. 实现代码**：
```rust
use ldap3::{LdapConn, Scope, SearchEntry};

pub struct LdapService {
    config: LdapConfig,
    pool: Arc<Mutex<Vec<LdapConn>>>,
}

impl LdapService {
    /// LDAP认证
    pub async fn authenticate(&self, username: &str, password: &str) -> Result<LdapUser> {
        // 1. 获取连接
        let mut ldap = LdapConn::new(&self.config.server)?;
        
        // 2. 使用服务账号绑定（查询用户DN）
        ldap.simple_bind(&self.config.bind_dn, &self.config.bind_password)?
            .success()?;
        
        // 3. 搜索用户
        let filter = self.config.user_filter.replace("{username}", username);
        let (rs, _res) = ldap.search(
            &self.config.base_dn,
            Scope::Subtree,
            &filter,
            vec![
                &self.config.attr_username,
                &self.config.attr_email,
                &self.config.attr_display_name,
                &self.config.attr_department,
                "dn",
            ]
        )?.success()?;
        
        if rs.is_empty() {
            return Err(Error::UserNotFound);
        }
        
        let entry = SearchEntry::construct(rs[0].clone());
        let user_dn = entry.dn;
        
        // 4. 使用用户凭据验证（实际认证）
        let mut user_ldap = LdapConn::new(&self.config.server)?;
        user_ldap.simple_bind(&user_dn, password)?
            .success()
            .map_err(|_| Error::InvalidCredentials)?;
        
        // 5. 返回用户信息
        Ok(LdapUser {
            username: entry.attrs.get(&self.config.attr_username)
                .and_then(|v| v.first())
                .ok_or(Error::MissingAttribute)?.clone(),
            email: entry.attrs.get(&self.config.attr_email)
                .and_then(|v| v.first())
                .cloned(),
            display_name: entry.attrs.get(&self.config.attr_display_name)
                .and_then(|v| v.first())
                .cloned(),
            department: entry.attrs.get(&self.config.attr_department)
                .and_then(|v| v.first())
                .cloned(),
        })
    }
    
    /// 用户同步（定时任务）
    pub async fn sync_users(&self, tenant_id: i64) -> Result<SyncReport> {
        let mut ldap = LdapConn::new(&self.config.server)?;
        ldap.simple_bind(&self.config.bind_dn, &self.config.bind_password)?
            .success()?;
        
        // 搜索所有用户
        let (rs, _) = ldap.search(
            &self.config.base_dn,
            Scope::Subtree,
            "(objectClass=person)",
            vec!["sAMAccountName", "mail", "displayName", "telephoneNumber"]
        )?.success()?;
        
        let mut report = SyncReport::default();
        
        for entry in rs {
            let entry = SearchEntry::construct(entry);
            let username = entry.attrs.get("sAMAccountName")
                .and_then(|v| v.first())
                .ok_or(Error::MissingAttribute)?;
            
            // 插入或更新数据库
            let result = sqlx::query!(
                r#"
                INSERT INTO users (tenant_id, extension, email, display_name, auth_method)
                VALUES ($1, $2, $3, $4, 'ldap')
                ON CONFLICT (tenant_id, extension) 
                DO UPDATE SET 
                    email = EXCLUDED.email,
                    display_name = EXCLUDED.display_name,
                    updated_at = NOW()
                "#,
                tenant_id,
                username,
                entry.attrs.get("mail").and_then(|v| v.first()),
                entry.attrs.get("displayName").and_then(|v| v.first()),
            ).execute(&self.db).await;
            
            match result {
                Ok(_) => report.synced += 1,
                Err(e) => {
                    report.failed += 1;
                    report.errors.push(format!("{}: {}", username, e));
                }
            }
        }
        
        Ok(report)
    }
}

// 数据库字段扩展
ALTER TABLE users ADD COLUMN auth_method VARCHAR(20) DEFAULT 'pin';
-- 值: 'pin', 'ldap', 'saml', 'oauth'
```

---

#### 二、SAML SSO集成

**1. 功能需求**：
- 支持SAML 2.0协议
- Service Provider (SP) 角色
- 对接企业Identity Provider（Okta、Azure AD、OneLogin）
- 单点登录/登出

**2. 依赖库**：
```toml
[dependencies]
samael = "0.0.14"       # SAML库
openssl = "0.10"        # 证书处理
```

**3. 配置文件**：
```toml
[saml]
enabled = true

# SP配置（RustPBX作为Service Provider）
entity_id = "https://pbx.company.com/saml/metadata"
acs_url = "https://pbx.company.com/saml/acs"  # Assertion Consumer Service
slo_url = "https://pbx.company.com/saml/slo"  # Single Logout

# IdP配置（企业Identity Provider）
idp_entity_id = "https://sso.company.com"
idp_sso_url = "https://sso.company.com/saml/sso"
idp_slo_url = "https://sso.company.com/saml/slo"
idp_cert_path = "/etc/rustpbx/certs/idp_cert.pem"

# SP证书（签名和加密）
sp_cert_path = "/etc/rustpbx/certs/sp_cert.pem"
sp_key_path = "/etc/rustpbx/certs/sp_key.pem"

# 属性映射
attr_user_id = "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/name"
attr_email = "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress"
attr_extension = "extension"  # 自定义属性
```

**4. 实现代码**：
```rust
use samael::metadata::{EntityDescriptor, ContactPerson};
use samael::service_provider::ServiceProviderBuilder;

pub struct SamlService {
    sp: ServiceProvider,
    config: SamlConfig,
}

impl SamlService {
    /// 初始化SAML SP
    pub fn new(config: SamlConfig) -> Result<Self> {
        let sp = ServiceProviderBuilder::default()
            .entity_id(&config.entity_id)
            .acs_url(&config.acs_url)
            .idp_metadata_url(&config.idp_metadata_url)
            .certificate_path(&config.sp_cert_path)
            .private_key_path(&config.sp_key_path)
            .build()?;
        
        Ok(Self { sp, config })
    }
    
    /// 生成SP元数据（提供给IdP配置）
    pub fn generate_metadata(&self) -> String {
        let descriptor = EntityDescriptor {
            entity_id: self.config.entity_id.clone(),
            sp_sso_descriptor: Some(/* ... */),
            // ...
        };
        descriptor.to_xml().unwrap()
    }
    
    /// 发起SSO登录（重定向到IdP）
    pub fn initiate_login(&self, relay_state: Option<String>) -> Result<String> {
        let authn_request = self.sp.create_authn_request()?;
        let redirect_url = self.sp.generate_redirect_url(&authn_request, relay_state)?;
        Ok(redirect_url)
    }
    
    /// 处理IdP回调（验证断言）
    pub async fn handle_acs(&self, saml_response: &str) -> Result<SamlUser> {
        // 1. 解析SAML响应
        let response = self.sp.parse_response(saml_response)?;
        
        // 2. 验证签名
        response.verify_signature(&self.config.idp_cert)?;
        
        // 3. 验证断言
        let assertion = response.assertion()?;
        assertion.verify_conditions()?;  // 时间窗口、受众
        
        // 4. 提取属性
        let attributes = assertion.attributes()?;
        let user_id = attributes.get(&self.config.attr_user_id)
            .ok_or(Error::MissingAttribute)?;
        let email = attributes.get(&self.config.attr_email);
        let extension = attributes.get(&self.config.attr_extension)
            .ok_or(Error::MissingExtension)?;
        
        Ok(SamlUser {
            user_id: user_id.clone(),
            email: email.cloned(),
            extension: extension.clone(),
        })
    }
}

// HTTP路由
#[get("/saml/metadata")]
async fn saml_metadata(saml: Data<SamlService>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/xml")
        .body(saml.generate_metadata())
}

#[get("/saml/login")]
async fn saml_login(saml: Data<SamlService>) -> HttpResponse {
    let redirect_url = saml.initiate_login(None).unwrap();
    HttpResponse::Found()
        .insert_header(("Location", redirect_url))
        .finish()
}

#[post("/saml/acs")]
async fn saml_acs(
    saml: Data<SamlService>,
    form: Form<SamlResponse>,
    auth_service: Data<VoicemailAuthService>,
) -> Result<HttpResponse> {
    // 1. 验证SAML响应
    let saml_user = saml.handle_acs(&form.saml_response).await?;
    
    // 2. 查询或创建用户
    let user = get_or_create_user(&saml_user).await?;
    
    // 3. 生成JWT token
    let token = auth_service.generate_token(&user)?;
    
    // 4. 设置cookie并重定向
    Ok(HttpResponse::Found()
        .cookie(Cookie::new("auth_token", token))
        .insert_header(("Location", "/voicemail"))
        .finish())
}
```

---

#### 三、OAuth 2.0 / OpenID Connect

**1. 适用场景**：
- 对接云服务（Google Workspace、Microsoft 365）
- 移动App社交登录
- 第三方集成

**2. 依赖库**：
```toml
[dependencies]
oauth2 = "4.4"
openidconnect = "3.0"
```

**3. 配置文件**：
```toml
[oauth]
enabled = true
provider = "azure"  # azure, google, okta, custom

# Azure AD示例
[oauth.azure]
client_id = "your-client-id"
client_secret = "your-client-secret"
tenant_id = "your-tenant-id"
redirect_uri = "https://pbx.company.com/oauth/callback"

# Scopes
scopes = ["openid", "profile", "email", "User.Read"]
```

**4. 实现代码**：
```rust
use openidconnect::{
    ClientId, ClientSecret, IssuerUrl, RedirectUrl,
    AuthorizationCode, TokenResponse, UserInfoClaims,
};

pub struct OAuthService {
    client: CoreClient,
}

impl OAuthService {
    /// 发起OAuth登录
    pub fn authorize_url(&self) -> (Url, CsrfToken) {
        self.client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("openid".to_string()))
            .add_scope(Scope::new("profile".to_string()))
            .add_scope(Scope::new("email".to_string()))
            .url()
    }
    
    /// 处理回调
    pub async fn handle_callback(&self, code: &str) -> Result<OAuthUser> {
        // 1. 交换code获取token
        let token_response = self.client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .request_async(async_http_client)
            .await?;
        
        // 2. 获取用户信息
        let id_token = token_response.id_token()
            .ok_or(Error::MissingIdToken)?;
        let claims = id_token.claims(&self.client.id_token_verifier(), nonce)?;
        
        Ok(OAuthUser {
            sub: claims.subject().to_string(),
            email: claims.email().map(|e| e.to_string()),
            name: claims.name().and_then(|n| n.get(None).cloned()),
        })
    }
}
```

---

#### 四、实施工作量评估

| 模块 | 工作项 | 工作量 | 依赖 |
|------|--------|--------|------|
| **LDAP集成** | | | |
| | 基础认证功能 | 3天 | ldap3库 |
| | 用户同步服务 | 2天 | 定时任务框架 |
| | 配置界面 | 2天 | Web UI |
| | 测试（AD+OpenLDAP） | 2天 | 测试环境 |
| **SAML SSO** | | | |
| | SP实现（元数据、ACS） | 4天 | samael库 |
| | IdP对接测试 | 3天 | Okta/Azure AD |
| | 证书管理 | 1天 | OpenSSL |
| | 单点登出（SLO） | 2天 | - |
| **OAuth/OIDC** | | | |
| | 基础OAuth流程 | 2天 | oauth2库 |
| | 多Provider支持 | 2天 | - |
| | Token管理 | 1天 | - |
| **通用功能** | | | |
| | 数据库schema扩展 | 1天 | - |
| | 用户映射逻辑 | 2天 | - |
| | 审计日志 | 1天 | - |
| | 文档编写 | 2天 | - |
| **总计** | | **30天** | 约1.5人月 |

**实施优先级**：
1. **Phase 1**（MVP）：LDAP认证 - 10天
2. **Phase 2**：SAML SSO - 10天
3. **Phase 3**：OAuth/OIDC - 10天

**技术风险**：
- SAML协议复杂性（证书、签名、时间同步）
- 各家IdP实现差异（需要适配）
- LDAP服务器版本兼容性
- 用户属性映射不一致

**建议**：
- 先实现LDAP（最常见、最简单）
- SAML作为高级功能（企业版）
- OAuth可选（主要用于云服务）

---

#### 4.1 留言列表
```
┌────────────────────────────────────────────────────────┐
│  语音信箱 - 分机 1001                      [设置] [帮助] │
├────────────────────────────────────────────────────────┤
│  [新留言 3]  [已读 12]  [已归档 45]  [已删除 5]        │
├────────────────────────────────────────────────────────┤
│  🔴 13800138000        10:23  42秒   项目讨论...  [播放]│
│  ⚪ 13900139000        09:15  1:23   紧急订单...  [播放]│
│  ⚪ 400-888-8888       昨天   2:05   客户投诉...  [播放]│
└────────────────────────────────────────────────────────┘
```

**功能**：
- 播放器（进度条、倍速、下载）
- 批量操作（标记已读、删除、导出）
- 搜索过滤（日期、号码、关键词）
- 分类标签（重要、紧急、客户、家人）

#### 4.2 留言详情
```
┌─────────────────────────────────────────────┐
│  留言详情                                    │
├─────────────────────────────────────────────┤
│  📞 来电号码：13800138000                    │
│  ⏰ 时间：2026-01-26 10:23:15               │
│  ⏱️  时长：42 秒                              │
│  📊 状态：未读                               │
├─────────────────────────────────────────────┤
│  🎵 音频播放                                 │
│  [▶] ━━━━━━━━━━━━━━━ 00:15 / 00:42        │
│  [1.0x] [下载 WAV] [下载 MP3]                │
├─────────────────────────────────────────────┤
│  📝 转写文本                                 │
│  你好啊，我是张三，今天下午想跟你聊一下      │
│  那个项目的事情，大概三点到四点之间有空吗？  │
│                                              │
│  🤖 AI 摘要                                  │
│  来电人：张三                                │
│  事由：项目讨论                              │
│  期望回电时间：今天下午 3-4 点               │
├─────────────────────────────────────────────┤
│  操作：                                      │
│  [标记已读] [删除] [归档] [回拨] [转发]      │
└─────────────────────────────────────────────┘
```

#### 4.3 个人设置
```
语音信箱设置
├─ 欢迎语
│  ├─ [x] 使用系统默认
│  ├─ [ ] 使用自定义录音
│  └─ [上传录音文件] [在线录制]
│
├─ 录音设置
│  ├─ 最大时长：[5] 分钟
│  ├─ 无声检测：[3] 秒自动结束
│  └─ 结束按键：[#]
│
├─ 通知设置
│  ├─ [x] 邮件通知：user@example.com
│  ├─ [ ] 短信通知：+86 138-0013-8000
│  ├─ [x] Web 推送
│  └─ [x] SIP MWI
│
├─ 转写设置
│  ├─ [x] 启用语音转写
│  ├─ [x] AI 智能摘要
│  └─ [ ] 关键词提取
│
└─ 配额管理
   ├─ 存储空间：2.3 GB / 10 GB
   ├─ 留言数量：68 条 / 无限制
   └─ 保留期限：90 天
```

---

#### 4.0.1 企业认证集成说明

> **重要**：LDAP/SSO/OAuth等企业认证功能由独立的 **Enterprise Auth 插件** 提供。  
> Voicemail Pro 通过调用全局认证服务获得以下能力。

**集成效果**：
- ✅ **LDAP/AD登录**：员工使用企业账号登录（如：zhangsan@company.com）
- ✅ **SAML SSO**：单点登录，从企业门户直接跳转，无需重复输入密码
- ✅ **OAuth/OIDC**：支持Google Workspace、Microsoft 365、Okta
- ✅ **MFA多因素认证**：强制二次验证（TOTP/SMS）
- ✅ **用户自动同步**：从LDAP/AD自动同步用户信息到Voicemail
- ✅ **权限继承**：继承企业AD/LDAP的用户权限和组织架构
- ✅ **统一安全策略**：密码策略、会话管理、审计日志全局统一

**插件依赖关系**：
```
Voicemail Pro ($499/年)
    ↓ 可选依赖
Enterprise Auth - 基础版 ($299/年)  ← LDAP/AD 认证
    或
Enterprise Auth - 企业版 ($799/年)  ← LDAP + SAML + OAuth + MFA
```

**配置示例**：
```toml
# config.toml
[voicemail]
# 指定使用全局认证服务
auth_provider = "enterprise_auth"  

[enterprise_auth]
enabled = true

# LDAP配置（基础版）
[enterprise_auth.ldap]
enabled = true
server = "ldap://ad.company.com:389"
base_dn = "OU=Users,DC=company,DC=com"
bind_dn = "CN=svcacct,DC=company,DC=com"
bind_password = "secret123"

# SAML配置（企业版）
[enterprise_auth.saml]
enabled = true
idp_sso_url = "https://sso.company.com/saml/sso"
sp_entity_id = "https://pbx.company.com"
```

**用户体验**：

1. **LDAP登录**：
   ```
   用户访问: https://pbx.company.com/voicemail
   → 输入企业账号: zhangsan 
   → 输入企业密码: ******
   → 后端调用LDAP验证
   → 登录成功，自动创建/更新用户记录
   ```

2. **SAML SSO登录**：
   ```
   用户访问: https://pbx.company.com/voicemail
   → 自动重定向到企业SSO页面
   → 企业统一认证（如已登录则跳过）
   → 回调到Voicemail，自动登录
   ```

3. **PIN码备用登录**（如果LDAP/SSO不可用）：
   ```
   用户访问: https://pbx.company.com/voicemail
   → 点击"使用PIN码登录"
   → 输入分机号 + PIN码
   → 本地验证（不依赖外部服务）
   ```

**实施建议**：
- Voicemail Pro **内置支持**分机PIN码认证（无需额外插件）
- 如需企业级认证，购买 Enterprise Auth 插件即可
- Enterprise Auth 可被所有模块复用（Console、IVR Designer、Call Center等）

---

### 5. 企业级功能

#### 5.1 配额管理
```sql
-- 租户配额
CREATE TABLE voicemail_quotas (
    tenant_id BIGINT PRIMARY KEY,
    max_storage_gb INT DEFAULT 100,        -- 最大存储
    max_messages_per_user INT DEFAULT 500, -- 每用户最大留言数
    retention_days INT DEFAULT 90,         -- 保留天数
    max_message_duration INT DEFAULT 300   -- 单条最长时长（秒）
);

-- 用户使用统计
CREATE TABLE voicemail_usage (
    user_id BIGINT PRIMARY KEY,
    message_count INT DEFAULT 0,
    storage_used_mb BIGINT DEFAULT 0,
    last_message_at TIMESTAMPTZ
);
```

#### 5.2 权限管理
```yaml
roles:
  - name: admin
    permissions:
      - voicemail.view_all      # 查看所有留言
      - voicemail.manage_quota  # 管理配额
      - voicemail.export        # 导出留言
      - voicemail.audit         # 审计日志
  
  - name: manager
    permissions:
      - voicemail.view_team     # 查看团队留言
      - voicemail.assign        # 分配留言
  
  - name: user
    permissions:
      - voicemail.view_own      # 查看自己的留言
      - voicemail.manage_own    # 管理自己的留言
```

#### 5.3 审计日志
```
2026-01-26 10:25:00 | user1001 | LISTEN   | voicemail_abc123
2026-01-26 10:26:15 | user1001 | DELETE   | voicemail_abc123
2026-01-26 10:30:22 | admin    | EXPORT   | 68 messages
2026-01-26 11:05:00 | user1002 | FORWARD  | voicemail_xyz789 -> user1003
```

#### 5.4 批量操作
- 批量删除（选择多条留言）
- 批量导出（ZIP 打包）
- 批量转发（分配给其他人）
- 批量标记（已读、重要）

---

## 🏗️ 技术架构

### 系统架构图

```
┌─────────────────────────────────────────────────────────┐
│                    Client Layer                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐             │
│  │SIP Phone │  │Web UI    │  │Mobile App│             │
│  └──────────┘  └──────────┘  └──────────┘             │
└─────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│                  RustPBX Core                           │
│  ┌───────────────────────────────────────────────────┐ │
│  │  SIP Proxy + Call Router                          │ │
│  └───────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│              Voicemail Application Server               │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐ │
│  │ VoicemailApp │  │ Notifier     │  │ Transcriber  │ │
│  │ (录音/播放)   │  │ (邮件/短信)   │  │ (语音转文字)  │ │
│  └──────────────┘  └──────────────┘  └──────────────┘ │
│                                                         │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐ │
│  │ Storage      │  │ MWI Manager  │  │ API Server   │ │
│  │ (文件管理)    │  │ (SIP NOTIFY) │  │ (REST/WS)    │ │
│  └──────────────┘  └──────────────┘  └──────────────┘ │
└─────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│                  Storage Layer                          │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐             │
│  │PostgreSQL│  │Local FS  │  │S3/MinIO  │             │
│  └──────────┘  └──────────┘  └──────────┘             │
└─────────────────────────────────────────────────────────┘
```

### 核心模块

#### 1. VoicemailApp（SIP 应用）
```rust
// src/addons/voicemail/app.rs
pub struct VoicemailApp {
    mode: VoicemailMode,
    user_id: i64,
    extension: String,
    storage: VoicemailStorage,
    transcriber: TranscriptService,
}

pub enum VoicemailMode {
    Record,  // 录制留言
    Check,   // 查询留言
}

#[async_trait]
impl CallApp for VoicemailApp {
    async fn on_enter(&mut self, controller: &mut CallController) -> Result<AppAction> {
        controller.answer().await?;
        
        match self.mode {
            VoicemailMode::Record => {
                self.record_message(controller).await?;
            }
            VoicemailMode::Check => {
                self.check_messages(controller).await?;
            }
        }
        
        Ok(AppAction::Continue)
    }
}
```

#### 2. Storage（存储层）
```rust
// src/addons/voicemail/storage.rs
pub struct VoicemailStorage {
    db: DbPool,
    fs: FileStorage,  // Local FS 或 S3
}

pub struct Message {
    pub id: Uuid,
    pub user_id: i64,
    pub caller: String,
    pub timestamp: DateTime<Utc>,
    pub duration: u32,
    pub audio_path: String,
    pub transcript: Option<String>,
    pub summary: Option<String>,
    pub status: MessageStatus,
}

pub enum MessageStatus {
    New,
    Read,
    Archived,
    Deleted,
}
```

#### 3. Notifier（通知服务）
```rust
// src/addons/voicemail/notifier.rs
pub struct VoicemailNotifier {
    email: EmailService,
    sms: SmsService,
    mwi: MwiService,
    websocket: WebSocketBroadcaster,
}

impl VoicemailNotifier {
    pub async fn notify_new_message(&self, message: &Message, user: &User) -> Result<()> {
        // 邮件通知
        if user.email_notification_enabled {
            self.email.send_voicemail_notification(message, user).await?;
        }
        
        // 短信通知
        if user.sms_notification_enabled {
            self.sms.send_voicemail_alert(message, user).await?;
        }
        
        // MWI 更新
        self.mwi.update_indicator(user.extension, user.unread_count).await?;
        
        // Web 推送
        self.websocket.broadcast_to_user(user.id, WebSocketMessage::NewVoicemail {
            message_id: message.id,
            caller: message.caller.clone(),
            duration: message.duration,
        }).await?;
        
        Ok(())
    }
}
```

---

## 📐 数据库设计

```sql
-- 留言表
CREATE TABLE voicemail_messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL REFERENCES users(id),
    caller VARCHAR(50) NOT NULL,
    callee VARCHAR(50) NOT NULL,
    
    -- 音频信息
    audio_path VARCHAR(1024) NOT NULL,
    audio_format VARCHAR(10) DEFAULT 'wav',
    duration INT NOT NULL,  -- 秒
    file_size BIGINT NOT NULL,  -- 字节
    
    -- 转写信息
    transcript TEXT,
    summary TEXT,
    keywords JSONB,  -- ["项目", "张三", "下午3点"]
    sentiment VARCHAR(20),  -- positive, neutral, negative, urgent
    
    -- 状态
    status VARCHAR(20) DEFAULT 'new',  -- new, read, archived, deleted
    is_important BOOLEAN DEFAULT false,
    tags VARCHAR(255)[],
    
    -- 时间戳
    created_at TIMESTAMPTZ DEFAULT NOW(),
    read_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ,
    
    -- 索引
    INDEX idx_user_status (user_id, status),
    INDEX idx_created_at (created_at),
    INDEX idx_caller (caller)
);

-- 用户配置
CREATE TABLE voicemail_configs (
    user_id BIGINT PRIMARY KEY REFERENCES users(id),
    
    -- 欢迎语
    greeting_type VARCHAR(20) DEFAULT 'default',  -- default, custom
    greeting_audio_path VARCHAR(1024),
    
    -- 录音设置
    max_duration INT DEFAULT 300,  -- 5分钟
    silence_timeout INT DEFAULT 3,  -- 3秒无声结束
    end_key VARCHAR(1) DEFAULT '#',
    
    -- 通知设置
    email_notification BOOLEAN DEFAULT true,
    email_address VARCHAR(255),
    sms_notification BOOLEAN DEFAULT false,
    sms_number VARCHAR(20),
    mwi_enabled BOOLEAN DEFAULT true,
    web_push_enabled BOOLEAN DEFAULT true,
    
    -- 转写设置
    transcript_enabled BOOLEAN DEFAULT true,
    summary_enabled BOOLEAN DEFAULT true,
    keyword_extraction BOOLEAN DEFAULT false,
    
    -- 配额
    max_messages INT DEFAULT 500,
    storage_quota_mb BIGINT DEFAULT 10240,  -- 10GB
    retention_days INT DEFAULT 90,
    
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

-- 使用统计
CREATE TABLE voicemail_stats (
    user_id BIGINT PRIMARY KEY REFERENCES users(id),
    message_count INT DEFAULT 0,
    unread_count INT DEFAULT 0,
    storage_used_mb BIGINT DEFAULT 0,
    last_message_at TIMESTAMPTZ,
    last_check_at TIMESTAMPTZ
);

-- 操作日志（审计）
CREATE TABLE voicemail_audit_logs (
    id BIGSERIAL PRIMARY KEY,
    tenant_id BIGINT NOT NULL,
    user_id BIGINT REFERENCES users(id),
    message_id UUID REFERENCES voicemail_messages(id),
    action VARCHAR(50) NOT NULL,  -- listen, delete, forward, export
    ip_address INET,
    user_agent TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    
    INDEX idx_user_action (user_id, action, created_at)
);
```

---

## 🚀 实现路线图

### Phase 1: MVP（2-3 周）

#### Week 1: 核心录音功能
- [x] 基础 VoicemailApp 实现
  - 录制留言（无人应答转接）
  - 播放留言（拨打 *97）
  - 删除留言
- [x] 数据库设计与迁移
- [x] 本地文件存储
- [x] 基础路由配置

**Milestone**: 可以录制和查询留言

#### Week 2: 通知与管理
- [x] 邮件通知（SMTP）
  - 新留言通知
  - 附件支持（WAV）
- [x] MWI（消息等待指示器）
  - SIP NOTIFY 实现
  - 支持主流话机
- [x] Web UI（基础版）
  - 留言列表
  - 播放器
  - 删除操作

**Milestone**: 完整的通知链路

#### Week 3: 转写与优化
- [x] 语音转写集成
  - 复用 Transcript 模块
  - 异步转写队列
- [x] 配额管理
  - 存储空间限制
  - 留言数量限制
- [x] License 验证框架
  - License Server 对接
  - 试用期管理

**Milestone**: MVP 完成，可商用

---

### Phase 2: 增强功能（1-2 周）

#### Week 4: 高级特性
- [ ] AI 智能摘要
- [ ] 关键词提取
- [ ] 情绪分析
- [ ] S3 存储支持
- [ ] 短信通知集成

#### Week 5: 企业功能
- [ ] 批量操作
- [ ] 权限管理
- [ ] 审计日志
- [ ] 导出功能（ZIP）
- [ ] 团队共享留言

**Milestone**: 企业级功能完整

---

### Phase 3: 移动端与优化（持续）

#### 后续迭代
- [ ] 移动端 App（iOS/Android）
- [ ] 语音指令操作（Siri/Google Assistant）
- [ ] 多语言支持
- [ ] 性能优化（大量留言场景）
- [ ] 高级分析（留言趋势、热门关键词）

---

## 💰 定价策略

### 订阅套餐

| 套餐 | 定价 | 功能 | 目标用户 |
|------|------|------|---------|
| **Basic** | $49/月 或 $499/年 | - 基础留言<br>- 邮件通知<br>- 10GB 存储<br>- 90天保留 | 小型企业 |
| **Pro** | $99/月 或 $999/年 | - Basic 全部<br>- 语音转写<br>- AI 摘要<br>- 短信通知<br>- 50GB 存储<br>- 1年保留 | 中型企业 |
| **Enterprise** | 定制 | - Pro 全部<br>- 无限存储<br>- 永久保留<br>- SLA 保障<br>- 专属支持 | 大型企业 |

### 按用户数计费

```
1-10 用户:   $49/用户/年
11-50 用户:  $39/用户/年（八折）
51-200 用户: $29/用户/年（六折）
200+ 用户:   $19/用户/年（四折）
```

### 增值服务

- **额外存储**: $10/月/100GB
- **短信通知包**: $20/月/1000条
- **高级 AI 功能**: $50/月（情绪分析、意图识别）
- **移动端 App**: $10/用户/月

---

## 🎯 竞品分析

### 主要竞品对比

| 功能 | RustPBX Voicemail Pro | FreePBX Voicemail | 3CX | Twilio Flex |
|------|----------------------|-------------------|-----|-------------|
| **基础留言** | ✅ | ✅ | ✅ | ✅ |
| **语音转写** | ✅ 本地化 | ❌ | ❌ | ✅ 云端 |
| **AI 摘要** | ✅ | ❌ | ❌ | ✅ |
| **邮件通知** | ✅ | ✅ | ✅ | ✅ |
| **短信通知** | ✅ | 🔶 第三方 | 🔶 第三方 | ✅ |
| **MWI** | ✅ | ✅ | ✅ | ❌ |
| **Web UI** | ✅ 现代化 | 🔶 老旧 | ✅ | ✅ |
| **移动端** | 🔄 开发中 | ❌ | ✅ | ✅ |
| **定价** | $499/年 | $395/年 | $695/年 | $1/用户/月 |
| **本地部署** | ✅ | ✅ | ✅ | ❌ |

### 差异化优势

1. **性能优势**：Rust 实现，低延迟高并发
2. **AI 集成**：本地化 AI（无隐私泄露）
3. **现代化 UI**：React 技术栈
4. **价格优势**：比 3CX 便宜 30%
5. **开源友好**：核心开源，商业插件可选

---

## 📊 成功指标（KPI）

### 技术指标
- **可用性**: >99.9%
- **录音成功率**: >99.5%
- **转写准确率**: >95%
- **通知到达率**: >99%
- **平均响应时间**: <200ms

### 业务指标
- **Q1 目标**: 50 个付费客户
- **Q2 目标**: 150 个付费客户
- **客单价**: $499/年
- **续费率**: >80%
- **NPS 评分**: >50

### 用户指标
- **日活跃用户**: 目标 1000+
- **每日新留言**: 目标 5000+
- **留言查询率**: >60%
- **邮件打开率**: >40%

---

## 🔒 风险与应对

### 技术风险
| 风险 | 影响 | 应对措施 |
|------|------|---------|
| 转写准确率不达标 | 高 | 多引擎切换（阿里云、讯飞备选） |
| 存储成本高 | 中 | S3 冷存储、自动清理 |
| 大量并发录音 | 中 | 异步处理、队列缓冲 |

### 业务风险
| 风险 | 影响 | 应对措施 |
|------|------|---------|
| 用户付费意愿低 | 高 | 免费试用 30 天、功能演示 |
| 竞品降价 | 中 | 强化差异化（AI、性能） |
| 法律合规（隐私） | 中 | GDPR 合规、数据加密 |

---

## 📞 支持与文档

### 用户文档
- 快速开始指南
- 功能使用手册
- 常见问题 FAQ
- 视频教程

### 开发者文档
- API 文档
- 插件开发指南
- 集成示例
- 故障排查

### 支持渠道
- 📧 邮件：support@miuda.ai
- 💬 在线客服（工作日 9:00-18:00）
- 📚 知识库：https://docs.rustpbx.com
- 🎟️ 工单系统

---

**下一步行动**：
1. ✅ 完成产品方案评审
2. ⏳ 启动 MVP 开发（预计 2-3 周）
3. ⏳ Beta 测试（5-10 个种子用户）
4. ⏳ 正式发布与推广
