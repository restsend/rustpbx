# RustPBX 配置概览

> 译者注：本篇忠实保留原文的加载说明和代码。原文关于默认文件、二进制旁目录和数据库模式的概括不等于所有加载分支的实际行为；当前主程序应显式传 `--conf`，相对路径按工作目录解析。具体接入路径已另见 [FreeSWITCH 接入方案](../freeswitch-extension-plan_zh.md)。

## 配置来源

RustPBX 按以下优先顺序加载配置：

1. **命令行参数**：例如 `--conf my_config.toml`。
2. **主配置文件**：`rustpbx.toml`（或自定义路径）。
3. **局部配置文件**：通过主配置定义的 glob 模式加载（例如 `trunks_files`、`routes_files`）。
4. **生成配置**：如果存在，则从 `generated_dir` 自动加载（由 UI/API 管理）。
5. **数据库配置**：当 `generated_db = true` 时，生成配置（中继、路由、队列、IVR、ACL、CC）存储在数据库的 `config_entries` 表中，而不是文件中。

## 存储模式

### 文件系统模式（默认）

默认情况下，系统假定二进制文件旁存在一个 `config` 文件夹：

```toml
[proxy]
# Root for generated configs (default: ./config)
generated_dir = "./config"
# Explicit overrides for file patterns
routes_files = ["config/routes/*.toml"]
trunks_files = ["config/trunks/*.toml"]
acl_files = ["config/acl/*.toml"]
```

### 数据库模式

设置 `generated_db = true`，将所有生成配置存入应用数据库：

```toml
[proxy]
generated_db = true
```

此模式下，系统读写 `config_entries` 表，而不是文件系统。包括：

- SIP 中继、路由、队列和 ACL 规则。
- IVR 项目定义（发布时）。
- CC ACD 配置、技能组和坐席。

数据库模式下忽略基于文件的 `routes_files`、`trunks_files` 等；所有配置通过数据库管理。`generated_dir` 路径也不再使用。

## 重载行为

修改 `rustpbx.toml` 通常需要重启。不过，**中继**、**队列**、**路由**和 **ACL** 可以通过管理控制台或 API 在运行时重载，而不中断活动通话。数据库模式同样支持运行时重载，只是配置从数据库加载。

## 插件系统

插件（例如 Wholesale、Queue、Transcript）在 `[proxy]` 节中启用。

```toml
[proxy]
addons = ["wholesale", "queue"]
```

## 配置参考

| # | 文件 | 主题 |
|---|---|---|
| 01 | [平台与网络](01-platform_zh.md) | HTTP/HTTPS、端口、TLS、外部 IP |
| 02 | [代理核心](02-proxy-core_zh.md) | SIP 协议栈、Realm、定位器、拨号计划 |
| 03 | [认证与用户](03-auth-users_zh.md) | 用户账号、密码、RBAC |
| 04 | [路由](04-routing_zh.md) | 路由规则、模式匹配 |
| 05 | [中继与队列](05-trunks-queues_zh.md) | SIP 中继、呼叫队列 |
| 06 | [媒体与录音](06-media-recording_zh.md) | RTP 代理、编解码器、通话录音、CDR |
| 07 | [插件、控制台与存储](07-addons-admin-storage_zh.md) | Web 控制台、AMI、Blob 存储、集群、许可证、RWI |
| 08 | [SipFlow](08-sipflow_zh.md) | SIP 流捕获、RTP 录音、存储后端、WAV 导出、REST API |
