# 插件、控制台与管理

> 译者注：代码和配置示例原样保留。原文将 AMI 描述为 Asterisk Manager Interface/TCP 事件流；此前核对本项目当前 AMI 实现为 HTTP 管理接口。此处保留原文并单独指出差异，避免把翻译当作实现保证。

## 管理控制台

内置 Web 管理界面。

控制台允许管理员监控系统健康状况和管理核心平台设置，无需手工编辑 TOML 文件。主要管理功能包括：

- **平台**：日志、外部 IP 和 RTP 端口范围。
- **代理设置**：Realm、用户认证后端、Locator Webhook 和 HTTP Router，并提供内置测试工具。
- **存储**：媒体录音路径和 S3 存储桶集成。
- **安全**：ACL 规则管理。

```toml
[console]
base_path = "/console"
# API prefix for REST endpoints (default: "/api")
api_prefix = "/api"
# Static files HTTP path prefix (default: "/static")
static_path = "/static"
session_secret = "change-me-random-string-must-be-long"

# Allow first user to create account?
allow_registration = false

# Cookie security
secure_cookie = false # Set true behind HTTPS proxy

# Default locale and available locales (i18n)
# locale_default = "en"
# [console.locales]
# en = { label = "English", flag = "🇬🇧" }
# zh = { label = "中文", flag = "🇨🇳" }

# API tokens for programmatic access to console REST APIs
# [[console.api_tokens]]
# token = "api-token-here"
# scopes = ["read", "write"]
# description = "Monitoring script token"

# Optional: override CDN URLs for frontend JS libraries.
# Configuring these to local paths or a nearby CDN mirror can significantly
# speed up page load / first-render time in restricted-network environments.
#
# Default values (loaded from public CDNs when not set):
#   alpine_js   = "https://cdnjs.cloudflare.com/ajax/libs/alpinejs/3.15.0/cdn.min.js"
#   tailwind_js = "https://cdnjs.cloudflare.com/ajax/libs/tailwindcss-browser/4.1.13/index.global.min.js"
#   chart_js    = "https://cdnjs.cloudflare.com/ajax/libs/Chart.js/4.5.0/chart.umd.min.js"
#   jssip_js    = "//jssip.net/download/releases/jssip-3.10.0.js"
#
# alpine_js   = "/static/js/alpine.min.js"
# tailwind_js = "/static/js/tailwind.min.js"
# chart_js    = "/static/js/chart.umd.min.js"
# jssip_js    = "/static/js/jssip.min.js"
```

## AMI（Asterisk Manager Interface）

用于兼容旧系统集成的 TCP 事件流。

```toml
[ami]
# IP whitelist
allows = ["127.0.0.1", "10.0.1.10"]
```

## 通用存储（`[storage]`）

由多个插件（转写、批发业务导出等）用于存储 Blob 数据。它与通话录音存储不同。

```toml
# Local Storage
[storage]
type = "local"
path = "storage/blobs"

# S3 Storage
# [storage]
# type = "s3"
# vendor = "aws"
# bucket = "app-assets"
# region = "us-west-2"
# access_key = "..."
# secret_key = "..."
```

## Archive 插件

自动归档旧数据。

```toml
[archive]
enabled = true
archive_time = "03:00:00"
retention_days = 90
```

## Wholesale 插件

在代理配置中启用该插件。

```toml
[proxy]
addons = ["wholesale"]
```

## 集群配置

用于高可用的多节点集群。每个节点都列出**所有**节点（包括自身；自身解析会将本地监听地址与此列表匹配）。

```toml
[cluster]
session_registry_backend = "db"   # "db" (default; shared PostgreSQL/MySQL),
                                   # "memory", or "noop"/"disabled"
session_registry_ttl_secs = 3600      # crashed-node reclaim window
session_registry_heartbeat_secs = 30  # per-node batch refresh interval

[[cluster.peers]]
addr = "10.0.0.1"
sip_port = 5060
ami_port = 8080

[[cluster.peers]]
addr = "10.0.0.2"
sip_port = 5060
ami_port = 8080
```

会话注册表用于回答“通话 X 属于哪个节点”。每个会话创建时注册所属节点（挂断时通过 RAII 注销）；Console 和 CC 的 REST 通话命令查询注册表，将控制请求转发给承载会话的节点，回退方式是向所有对等节点广播。`GET {ami_path}/cluster/session_owner/{call_id}` 用于解析通话所属节点（commerce 构建）。

## 商业许可证

将商业插件映射到命名许可证密钥。

```toml
[licenses]
[licenses.addons]
wholesale         = "enterprise"
voicemail         = "basic"
ivr_editor        = "enterprise"

[licenses.keys]
enterprise = "LICENSE-KEY-XXXX-XXXX-XXXX"
basic      = "LICENSE-KEY-YYYY-YYYY-YYYY"
```

## RWI（实时 WebSocket 接口）

RWI 子系统提供基于 WebSocket 的 JSON 通话控制。只要存在 `[rwi]` 节就会启用 RWI。完整协议参考见 [RWI 协议](../rwi_zh.md)。

```toml
[rwi]
max_connections = 2000
max_calls_per_connection = 200
orphan_hold_secs = 30
originate_rate_limit = 10

# API tokens for RWI access
[[rwi.tokens]]
token = "my-static-token"
scopes = ["call", "session", "media", "record", "conference", "queue"]
```

## RWI Webhook（`[rwi_webhook]`）

用于 RWI 实时事件（通话开始、通话结束、媒体录制等）的顶层 Webhook。结构与 `[proxy.locator_webhook]` 相同。

```toml
[rwi_webhook]
url = "https://events.example.com/rwi"
events = ["call.started", "call.ended", "media.recorded"]
headers = { "Authorization" = "Bearer token123" }
timeout_ms = 5000
```
