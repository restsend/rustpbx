# 平台与网络

> 译文说明：代码与配置示例（含注释）原样保留。以下参数说明忠实对应英文原文，不代表本次已核验所有默认值。

## HTTP 与 HTTPS

配置用于 API、管理控制台和 Webhook 处理的内部 Web 服务器。

```toml
# Main listener
http_addr = "0.0.0.0:8080"
# Enable GZIP compression for HTTP responses
http_gzip = true

# Optional: HTTPS listener
https_addr = "0.0.0.0:8443"
ssl_certificate = "./certs/fullchain.pem"
ssl_private_key = "./certs/privkey.pem"

# Security: Skip access logs for health checks or metrics
http_access_skip_paths = ["/health", "/metrics"]
```

## 日志

全局日志配置。

```toml
# Levels: debug, info, warn, error
log_level = "info"

# If unset, logs to stderr
log_file = "/var/log/rustpbx/app.log"

# Log rotation policy (only effective when log_file is set).
# Allowed values:
#   "never"  – single file, no rotation (default)
#   "daily"  – rotate once per day; filename suffix: YYYY-MM-DD
#   "hourly" – rotate once per hour; filename suffix: YYYY-MM-DD-HH
log_rotation = "daily"
```

> **关于 `log_file` 与轮转**：`log_file` 被视为文件名前缀。例如，设置 `log_file = "/var/log/rustpbx/app.log"` 和 `log_rotation = "daily"` 时，实际写入的文件为 `/var/log/rustpbx/app.log.2026-04-10`。进程启动前，该目录必须已存在且可写。旧的轮转文件**不会**自动删除；请使用 `logrotate` 或类似工具实施保留策略。

## 媒体缓存

媒体文件（例如回铃音、IVR 提示音）的本地缓存目录。通过 Console UI 管理，不会解析到 `Config` 结构体中。

```toml
media_cache_path = "./config/mediacache"
```

## 数据库

主数据库连接。支持 SQLite、PostgreSQL 和 MySQL。

```toml
# SQLite (default)
database_url = "sqlite://rustpbx.sqlite3"

# PostgreSQL
# database_url = "postgres://user:pass@localhost:5432/rustpbx"

# MySQL
# database_url = "mysql://root@localhost:3306/rustpbx"
```

### 数据库连接池

控制 PostgreSQL/MySQL 数据库的连接池。SQLite 使用单连接，忽略这些设置。省略本节时，`max_connections` 默认为 64。

```toml
[database_pool]
max_connections = 64       # Maximum pool size (default: 64)
min_connections = 0        # Minimum idle connections
acquire_timeout_secs = 30  # Timeout in seconds to acquire a connection from pool
idle_timeout_secs = 600    # Max idle time (seconds) before closing; None = no limit
max_lifetime_secs = 1800   # Max connection lifetime (seconds); None = no limit
```

## 演示模式

```toml
# When true, a demo superuser account is auto-created on startup
# and some addons run in evaluation mode (e.g. ACME bypasses
# certificate verification).
demo_mode = false
```

## 网络与 NAT

RustPBX 将**媒体（RTP/SDP）**和**信令（SIP Contact）**的外部地址分开配置，以避免 BYE 被发送到无法到达 PBX 进程的公网 IP，造成局域网 NAT 回流失败（#244）。

### RTP / SDP（媒体）

```toml
# Public IP advertised in SDP c=/o= and ICE candidates
external_ip = "203.0.113.10"

# Auto-detect RTP external IP (mutually exclusive with external_ip)
# auto_external_ip = "http://ifconfig.me"

# Defaults: 12000–42000 (~30k ports; plan roughly 2 RTP ports per concurrent call)
rtp_start_port = 12000
rtp_end_port = 42000
# webrtc_port_start = 30000
# webrtc_port_end = 40000
```

> **配置档 `bind_ip` 的回退**：配置档省略 `bind_ip` 时，运行时使用全局 RTP 绑定地址（目前为 `[proxy].addr`），然后再显式回退到 `[proxy].addr`。

### SIP Contact（信令）

```toml
# Optional dedicated Contact host for WAN peers (defaults to external_ip when unset)
# sip_external_ip = "203.0.113.10"
# auto_sip_external_ip = "http://ifconfig.me"

# Always use [proxy].addr in Contact (pure LAN / no NAT hairpin)
# sip_contact_always_bind = true

# CIDR list for "local" peers (empty = RFC1918 + loopback defaults)
# local_networks = ["192.168.0.0/16", "10.0.0.0/8"]

# When true (default), LAN destinations get bind address in Contact
contact_lan_use_bind = true
```

这两部分都可通过 **Console → Settings → Platform** 配置（RTP 外部 IP、SIP Contact IP、本地网络）。

### 网络配置档（多出口）

当部署存在多个出口（公网 WAN、Tailscale/WireGuard 覆盖网络等）时，定义命名配置档并将中继绑定到它们。当 `[[network_profile]]` 为空时，会根据上面的顶层字段生成一个 `default` 配置档。

```toml
default_network_profile = "wan"

[[network_profile]]
id = "wan"
label = "Public WAN"
external_ip = "203.0.113.10"
sip_external_ip = "203.0.113.10"
local_networks = ["192.168.0.0/16"]
rtp_start_port = 12000
rtp_end_port = 42000

[[network_profile]]
id = "overlay"
label = "Tailscale"
external_ip = "100.64.0.5"
bind_ip = "100.64.0.5"
contact_lan_use_bind = true
```

配置档省略 `bind_ip` 时，运行时使用全局 RTP 绑定地址（当前与 `[proxy].addr` 相同），不会强制设置不同的值。

中继可通过 TOML 的 `profile = "overlay"` 或控制台中继的 **Media Option → Network profile** 引用配置档。如果同时设置了中继级 `external_ip` / `bind_ip`，它们仍会覆盖配置档的值。

通过 **Console → Settings → Network profiles** 管理配置档。

### ICE 服务器

```toml
[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[ice_servers]]
urls = ["turn:turn.example.com:3478"]
username = "myuser"
credential = "mypassword"

# ice_servers_path = "/iceservers"
```
