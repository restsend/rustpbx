# SipFlow Todo List

## 1. 基础架构搭建 (Core Infrastructure)
- [x] 初始化 Rust 项目，配置 `tokio`, `rusqlite`, `serde`, `axum` 等依赖
- [x] 实现基础日志逻辑 (tracing/log)
- [x] 配置文件解析与命令行参数实现 (`--addr`, `--port`, `--data-dir`)

## 2. 接收层 (Reception Layer)
- [x] 实现异步 UDP Server (默认端口 3000)
- [x] 定义通用消息头部解析协议 (msgtype: SIP/RTP, src_ip, dst_ip, payload)
- [x] 建立异步处理逻辑并对接存储引擎

## 3. 存储引擎 (Storage Engine)
- [x] **小时级分片逻辑**:
    - [x] 动态创建目录结构 `storage/YYYYMMDD/HH/`
    - [x] 实现自动 Rotate 机制：跨整点切换 SQLite 连接和 RAW 文件句柄
- [x] **RAW 文件块压缩**:
    - [x] 集成 `zstd` 库
    - [x] 实现 `Header [Magic][OrigSize][CompressedSize] + Data` 写入格式
    - [x] 支持基于 `offset` 的随机读取与解压 (框架已搭好)
- [x] **元数据持久化**:
    - [x] SQLite 表结构设计 (`sip_msgs`, `media_msgs`)
    - [x] 实现批量写入机制：达到 200 条或 5 秒超时自动触发 `COMMIT`
    - [x] 实现 `Drop` 自动 Flush，防止进程退出丢失数据

## 4. 业务逻辑与音频处理 (Business & Audio Logic)
- [x] **SIP 解析**:
    - [x] 提取 `Call-ID`
    - [x] 解析 SDP 信息：提取音频负载类型 (PT)、IP、端口
- [ ] **音频还原 (WAV Export)**:
    - [x] 基于 `Call-ID` 和时间戳匹配媒体流 (RTP)
    - [ ] 实现 RTP 乱序重排 (目前仅按存储顺序/时间戳排序)
    - [x] 转码器集成:
        - [x] PCMA/PCMU 直接封装 WAV (16bit PCM)
        - [x] G.729 解码插件集成 (经 `audio-codec` 转 PCM)
        - [x] Opus 解码插件集成 (经 `audio-codec` 转 PCM)
    - [x] 生成标准 `.wav` 文件并支持下载 (支持 48k/8k 动态采样率)

## 5. HTTP API 接口
- [x] `GET /health`: 服务状态与基础健康检查
- [x] `GET /flow?callid=xxx&start=xxx&end=xxx`: 查询跨小时信令流程
- [x] `GET /media?callid=xxx&start=xxx&end=xxx`: 下载对应通话的音频文件 (.wav)

## 6. 监控与指标 (Metrics)
- [x] 实现 Prometheus 指标收集
- [x] 指标项包含：
    - [x] `sipflow_packets_received_total`
    - [ ] `sipflow_packets_dropped_total`
    - [x] `sipflow_db_insert_latency_ms`
    - [x] `sipflow_raw_write_bytes_total`

## 7. 性能优化 (Performance & Optimization)
- [x] **数据库索引**: 为 `media_msgs` 添加 `timestamp` 和 `(src, dst)` 复合索引以支持快速检索
- [x] **数据安全**: 增加启动时 `data_dir` 强制创建检查
- [x] **跨小时检索**: 实现 Directory Scanner 支持多小时分片合并查询
- [ ] 开放 `/metrics` 接口

## 7. 测试与优化
- [ ] 编写 UDP 模拟压力测试脚本
- [ ] 验证跨小时切换数据库的原子性
- [ ] 验证在高负载下 SQLite WAL 模式的性能表现

## 8. SDK 消息格式规范 (SDK Protocol Specification)
为了确保高性能传输，客户端与 SipFlow 之间建议采用紧凑的二进制协议（UDP）：

| 字段 | 类型 | 长度 | 说明 |
| :--- | :--- | :--- | :--- |
| **Magic Header** | u16 | 2 bytes | 固定为 `0x5346` (ASCII 'SF') |
| **Version** | u8 | 1 byte | 协议版本，当前为 `0x01` |
| **MsgType** | u8 | 1 byte | `0`: SIP, `1`: RTP |
| **IP Family** | u8 | 1 byte | `4`: IPv4, `6`: IPv6 |
| **Src IP** | bytes | 4/16 bytes | 原始发送方 IP |
| **Src Port** | u16 | 2 bytes | 原始发送方 端口 |
| **Dst IP** | bytes | 4/16 bytes | 原始接收方 IP |
| **Dst Port** | u16 | 2 bytes | 原始接收方 端口 |
| **Timestamp** | u64 | 8 bytes | 微秒级 Unix 时间戳 (Big Endian) |
| **Payload Len**| u32 | 4 bytes | 后续 Payload 的实际长度 |
| **Payload** | bytes | Variable | 原始 SIP 报文或 RTP 报文全文本 |

> **注意**: 所有多字节整数均采用网络字节序 (Big Endian)。如果是 IPv4，IP 字段固定占 4 字节；如果是 IPv6，则占 16 字节。
