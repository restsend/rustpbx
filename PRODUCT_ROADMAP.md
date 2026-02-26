# RustPBX 商业化产品路线图

本文档规划了 RustPBX 向**商业化 IP PBX** 演进的插件开发方向与优先级策略。

---

## 📊 现有模块状态评估

### ✅ 已实现核心功能（Community/Built-in）
- **SIP Proxy** - 完整的 SIP 协议栈（UDP/TCP/WebSocket）
- **Media Proxy** - RTP/RTCP 媒体代理与 NAT 穿透
- **Queue Manager** - 呼叫队列与 ACD 分配（Sequential/Parallel）
- **ACME** - 自动化 SSL 证书管理（Let's Encrypt）
- **Transcript** - 通话录音转写（SenseVoice 本地化）
- **Archive** - CDR 归档管理
- **SipFlow** - 统一 SIP+RTP 录制系统（优于传统录音的 I/O 性能）

### ✅ 已实现商业功能
- **Wholesale** ($4999/年) - 批发/中继运营商管理
  - Trie 树高性能路由匹配
  - 多供应商费率管理
  - 实时计费与 CDR 流式归档
  - 租户多租户支持

---

## 🎯 商业化插件开发规划

### 🔥 P0 优先级（Q1-Q2 2026）

#### 0. **Enterprise Auth**（企业认证插件）🔐
**市场需求**：企业安全合规的基础（⭐⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐  
**目标定价**：$299/年（基础版）/ $799/年（企业版）

**核心价值**：
- **全局认证层**：统一的身份认证服务，被所有模块复用
- **企业合规**：满足企业IT安全策略（SSO强制、密码策略）
- **降低管理成本**：员工离职时在AD/LDAP删除即可

**核心功能**：
- [ ] **LDAP/AD 集成**（基础版）
  - 用户认证（替代本地密码）
  - 用户信息同步（姓名、邮箱、部门、分机号）
  - 定时同步（每6小时）
  - 支持Active Directory、OpenLDAP
- [ ] **SAML 2.0 SSO**（企业版）
  - Service Provider角色
  - 对接Okta、Azure AD、OneLogin
  - 单点登录/登出
  - 自动用户Provision
- [ ] **OAuth 2.0 / OpenID Connect**（企业版）
  - Google Workspace、Microsoft 365
  - 社交登录（可选）
  - API Token管理
- [ ] **多因素认证（MFA）**（企业版）
  - TOTP（Google Authenticator）
  - SMS验证码
  - Email验证码
- [ ] **统一用户管理**
  - Console管理界面认证
  - Voicemail Web认证
  - API访问认证
  - 报表系统认证
  - WebRTC客户端认证
- [ ] **审计日志**
  - 登录日志（成功/失败/IP/User-Agent）
  - 权限变更日志
  - 异常行为检测

**技术架构**：
```rust
// 全局认证服务
pub struct AuthService {
    ldap: Option<LdapProvider>,
    saml: Option<SamlProvider>,
    oauth: Option<OAuthProvider>,
    local: LocalAuthProvider,  // 回退方案
}

// 所有模块通过统一接口认证
trait Authenticator {
    async fn authenticate(&self, credentials: Credentials) -> Result<User>;
    async fn verify_token(&self, token: &str) -> Result<User>;
}
```

**被集成的模块**：
- Console管理后台
- Voicemail Pro Web界面
- IVR Designer管理界面
- Call Center Agent Portal
- Analytics Dashboard
- API访问（Bearer Token）
- SIP客户端注册（可选）

**实施优先级**：
- Phase 1（基础版，2周）：LDAP认证 + Console集成
- Phase 2（企业版，2周）：SAML SSO + 多模块集成
- Phase 3（高级版，1周）：OAuth + MFA

---

#### 1. **Endpoint Manager**（终端自动配置）🔌
**市场需求**：MSP 与集成商的核心痛点（⭐⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐  
**目标定价**：$149/年 (独立) / 包含在 Professional 版

**核心价值**：
- **零接触部署**：话机插网线即可自动配置
- **多品牌支持**：统一管理 Yealink, Fanvil, Cisco 等品牌
- **降低运维成本**：无需逐台配置 Web 界面

**核心功能**：
- [ ] **品牌/型号模板管理**
  - 支持 Yealink, Fanvil, Grandstream, Cisco, Polycom
  - 可视化按键（DSS Key）配置
- [ ] **设备发现与映射**
  - 网络扫描（ARP/DHCP Snooping）
  - MAC 地址绑定分机
- [ ] **Provisioning Server**
  - HTTP/HTTPS/TFTP 服务
  - 动态生成 XML/CFG 配置文件
- [ ] **固件管理**
  - 固件上传与分发
  - 定时更新
- [ ] **远程控制**
  - 远程重启
  - 恢复出厂设置

---

#### 2. **CRM Integration**（CRM 集成插件）
**市场需求**：销售团队的生产力工具（⭐⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐  
**目标定价**：$399/年（按 CRM 系统单独收费）  
**依赖**：Enterprise Auth 插件（用于用户身份同步）

**核心功能**：
- [ ] 来电弹屏（Screen Pop）
  - WebSocket 实时推送
  - 显示客户信息（姓名、历史订单、最后联系时间）
- [ ] 自动记录通话到 CRM
  - Salesforce / HubSpot / Zoho / 纷享销客
  - 通话时长、录音链接
- [ ] 点击拨号（Click-to-Call）
  - Chrome 插件 / Web 界面
- [ ] 通话录音自动上传到客户记录
- [ ] 通话转写文本同步

**支持 CRM 列表**：
- Phase 1: Salesforce, HubSpot
- Phase 2: Zoho, Pipedrive, 纷享销客, 销售易

---

### 🟡 P1 优先级（Q3-Q4 2026）

#### 3. **Analytics & Dashboard**（实时监控与报表）
**市场需求**：运营团队的决策支持（⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐  
**目标定价**：$699/年

**核心功能**：
- [ ] 实时通话仪表盘
  - 当前并发数
  - ASR（接通率）/ ACD（平均通话时长）
  - 实时告警（并发超限、ASR 异常）
- [ ] 队列性能分析
  - 平均等待时长
  - 放弃率（Abandoned Rate）
  - 服务水平（SLA）
- [ ] 话务员绩效报表
  - 接听量、通话时长、空闲时间
- [ ] 费用趋势与账单预测（Wholesale 场景）
- [ ] 可定制的 Grafana/Prometheus 集成
- [ ] 历史数据挖掘（月/季度对比）
- [ ] 导出报表（PDF/Excel）

---

#### 4. **Voicemail Pro**（语音信箱）
**市场需求**：企业 PBX 的标准功能（⭐⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐  
**目标定价**：$499/年 或 $49/月

**核心功能**：
- [ ] 分机/用户级语音信箱（独立配额管理）
- [ ] 留言存储与播放（WAV/Opus 格式）
- [ ] 可视语音留言（录音自动转写）
- [ ] Email 通知（新留言推送附件）
- [ ] SMS 通知集成
- [ ] MWI（Message Waiting Indicator）支持
- [ ] Web 管理界面（播放、删除、标记已读）
  - **复用Enterprise Auth插件**：LDAP/SSO登录
  - 分机PIN码作为备用认证方式
- [ ] 移动端 App 支持（可选）

**技术栈建议**：
- 存储：Local FS + S3 可选
- 转写：复用 Transcript 模块
- 通知：SMTP + Twilio/阿里云短信
- 认证：**依赖 Enterprise Auth 插件**

---

#### 5. **IVR Designer**（可视化 IVR 流程设计器）
**市场需求**：中小企业自助服务的核心（⭐⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐⭐  
**目标定价**：$999/年  
**依赖**：Enterprise Auth 插件（用于管理界面认证）

**核心功能**：
- [ ] 拖拽式 IVR 流程设计（React Flow / Vue Flow）
- [ ] DTMF 按键识别与分支跳转
- [ ] TTS（文本转语音）集成
  - Azure TTS / Google TTS / 阿里云 TTS
  - 本地化离线 TTS（可选）
- [ ] ASR（语音识别）集成（可选）
- [ ] 时间条件路由（工作日/节假日/自定义时段）
- [ ] 变量与数据库查询（客户信息查询）
- [ ] HTTP API 外部集成（CRM/ERP 数据查询）
- [ ] 流程模板库（客服、销售、技术支持等）
- [ ] 流程版本管理与回滚
- [ ] 实时调试与日志追踪

**技术栈建议**：
- 前端：React + React Flow
- 后端：状态机引擎（自研或集成 workflow engine）
- 音频：复用 Media 模块

---

### 🟢 P2 优先级（2027）

#### 6. **Call Center Suite**（呼叫中心套件）
**市场需求**：大型客服中心（⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐⭐  
**目标定价**：$2999/年（企业级）

**核心功能**：
- [ ] 智能路由（技能组匹配）
  - 语言技能
  - 产品专家
  - VIP 客户优先级
- [ ] 回拨队列（Callback Queue）
  - 用户留号等待
  - 系统自动回拨
- [ ] 质检与录音抽查
  - 随机抽样
  - 关键词匹配（敏感词检测）
- [ ] 实时监听/耳语/插入（Supervisor 功能）
  - 监听：只听
  - 耳语：只对话务员说话
  - 插入：三方通话
- [ ] Wallboard（队列墙板）
  - 大屏展示当前队列状态
- [ ] 排班管理
  - 话务员班次管理
  - 自动切换技能组

---

#### 7. **Conference Bridge**（多方会议）
**市场需求**：远程办公必备（⭐⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐  
**目标定价**：$799/年

**核心功能**：
- [ ] 固定/动态会议室管理
- [ ] PIN 码保护
- [ ] 主持人控制（静音、踢出、锁定会议室）
- [ ] 会议录音与实时转写
- [ ] WebRTC 视频会议支持（可选）
- [ ] 会议室预订系统（日历集成）
- [ ] 会议纪要自动生成（AI 摘要）
- [ ] 多会议室并发支持

---

#### 8. **Multi-Tenant Management**（多租户管理）
**市场需求**：MSP（托管服务提供商）（⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐⭐  
**目标定价**：$1999/年

**核心功能**：
- [ ] 租户隔离（独立的 Realm、路由、存储）
- [ ] 白标（White Label）定制
  - 自定义 Logo/主题
  - 独立域名
- [ ] 资源配额管理
  - 并发数限制
  - 存储配额
  - 录音保留期限
- [ ] 计费与发票生成
  - 按分钟计费
  - 按功能模块收费
- [ ] 租户自助面板
  - 自助开通/关闭功能
  - 账单查询
- [ ] 租户间隔离审计

---

#### 9. **Security & Fraud Detection**（反欺诈与安全）
**市场需求**：运营商与企业的安全合规（⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐  
**目标定价**：$899/年

**核心功能**：
- [ ] 异常流量检测
  - 短时大量呼叫（防机器人）
  - 异常呼叫时长
  - 高费率目的地异常
- [ ] IP/前缀黑名单管理
  - 自动封禁
  - 手动添加/删除
- [ ] STIR/SHAKEN 认证（北美市场）
- [ ] 地理围栏（限制来源/目的地国家）
- [ ] 实时告警
  - Slack / Email / 钉钉
  - Webhook 集成
- [ ] 安全审计日志

---

#### 10. **Fax Gateway**（T.38 传真网关）
**市场需求**：医疗、法律等行业（⭐⭐）  
**技术复杂度**：⭐⭐⭐⭐  
**目标定价**：$599/年

**核心功能**：
- [ ] T.38 协议支持
- [ ] 传真转 Email（PDF 格式）
- [ ] Email 转传真
- [ ] 传真日志与状态追踪
- [ ] 传真归档管理
- [ ] OCR 识别（可选）

---

#### 11. **Backup & Disaster Recovery**（备份与灾难恢复）
**市场需求**：企业级可靠性保障（⭐⭐⭐）  
**技术复杂度**：⭐⭐⭐  
**目标定价**：$399/年

**核心功能**：
- [ ] 自动备份（Config、DB、录音）
- [ ] 异地备份（S3/MinIO/阿里云 OSS）
- [ ] 一键恢复
- [ ] 配置版本管理
- [ ] 增量备份
- [ ] 定时任务（Cron）
- [ ] 备份健康检查

---

## 💰 商业化定价策略

### 套餐方案

| 套餐名称 | 包含插件 | 定价 | 目标客户 |
|---------|---------|------|---------|
| **Community** | 核心功能 + Queue + Transcript | 免费 | 个人/小团队 |
| **Basic** | Community + Voicemail Pro | $999/年 | 小型企业（<50 用户）|
| **Professional** | Basic + IVR + Conference + CRM Integration | $2499/年 | 中型企业（50-200 用户）|
| **Enterprise** | Professional + Call Center Suite + Analytics | $4999/年 | 大型企业（200+ 用户）|
| **Carrier** | Wholesale + Multi-Tenant + Security + Analytics | $9999/年 | 运营商/MSP |

### 捆绑销售（Bundle）

- **Office Suite**：Voicemail + IVR + Conference → $1799/年（省 $399）
- **Sales Suite**：CRM Integration + Analytics → $899/年（省 $199）
- **Operator Suite**：Wholesale + Security + Analytics → $6999/年（省 $899）

---

## 📈 收入预测（2026-2027）

### 目标客户分层
- **Basic**：100 客户 × $999 = $99,900
- **Professional**：50 客户 × $2499 = $124,950
- **Enterprise**：20 客户 × $4999 = $99,980
- **Carrier**：5 客户 × $9999 = $49,995

**年度目标收入**：$374,825

---

## 🛠 技术架构建议

### 插件开发规范
1. **独立 Git 仓库**（Git Submodule）
2. **Cargo Feature Flags** 控制编译
3. **License 验证**（商业插件）
4. **统一的 Addon Trait** 接口
5. **API 优先**（RESTful + WebSocket）
6. **前后端分离**（Vue 3 / React）

### 基础设施需求
- **License Server**：验证商业许可证
- **Update Server**：插件自动更新
- **Documentation Site**：完善的文档与 API 参考
- **Demo Instance**：在线演示环境

---

## 🎓 市场竞争分析

### 主要竞争对手
1. **FreePBX** - 开源，但商业模块收费
2. **3CX** - 闭源，按并发数收费
3. **Asterisk** - 开源，但需要定制开发
4. **FusionPBX** - 开源，FreeSWITCH 前端

### RustPBX 的差异化优势
- ✅ **性能**：Rust 原生，低延迟高并发
- ✅ **安全**：内存安全，无 Buffer Overflow
- ✅ **现代化**：WebRTC、SipFlow、AI 集成
- ✅ **可扩展**：插件化架构，易于定制
- ✅ **开源友好**：核心开源，商业插件可选

---

## 📞 联系与支持

- **官网**：https://rustpbx.com
- **商业咨询**：sales@miuda.ai
- **技术支持**：support@miuda.ai
- **文档**：https://docs.rustpbx.com

---

**最后更新**：2026-02-12
