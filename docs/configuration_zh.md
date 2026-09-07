# RustPBX 配置指南

> 译者注：下文保留原文关于默认配置文件的描述；此前核对当前主程序发现，不传 `--conf` 会使用内置默认配置。部署时请显式指定 `--conf`；这条差异说明不修改英文原文。

RustPBX 配置按功能划分为多个逻辑章节。应用默认从 `rustpbx.toml` 加载主配置，也可以通过 `--conf` 参数指定其他路径。配置格式为 **TOML**。

## 导航

1. [总览与概念](config/00-overview_zh.md)  
   *文件结构、重载行为、生成配置、数据库配置模式。*

2. [平台与网络](config/01-platform_zh.md)  
   *HTTP、日志、数据库、RTP、NAT、ICE。*

3. [代理核心](config/02-proxy-core_zh.md)  
   *绑定端口、传输协议（UDP/TCP/TLS/WS）、并发、模块。*

4. [认证与用户](config/03-auth-users_zh.md)  
   *用户后端（内存、数据库、HTTP）、定位器、Realm。*

5. [路由](config/04-routing_zh.md)  
   *静态路由、正则匹配、改写、HTTP 动态路由器。*

6. [中继与队列](config/05-trunks-queues_zh.md)  
   *SIP 网关、负载均衡、队列策略、坐席管理。*

7. [媒体、录音与 CDR](config/06-media-recording_zh.md)  
   *媒体代理、录音策略、存储后端（本地/S3）。*

8. [插件、控制台与管理](config/07-addons-admin-storage_zh.md)  
   *Web 控制台、AMI、归档、批发业务和自定义插件。*

9. [SipFlow](config/08-sipflow_zh.md)  
   *SIP 信令捕获、RTP 录音、存储后端、WAV 导出。*

10. [IVR Exec — 通话中插入 IVR](ivr_exec.md)  
    *SIP INFO 命令参考、Webhook 回调、cc-phone SDK 接入、数据结构。*
