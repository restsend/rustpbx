# 可观测性

> 译者注：本文完整翻译自 `observability.md`；代码、配置、指标名、字段名与命令保持原样。

RustPBX 支持两个层级的可观测性，由 Cargo feature flag 控制：

| 层级 | Feature flag | 提供的能力 |
|---|---|---|
| 社区版 | `addon-observability`（默认） | Prometheus 抓取端点 + 存活探针 |
| 商业版 | `addon-telemetry`（取代社区版） | Prometheus + OpenTelemetry 链路追踪（OTLP/gRPC） |

两个层级在编译时互斥。  
启用 `addon-telemetry` 时，它会安装与社区版相同的 Prometheus recorder 和端点，**另外**提供完整的分布式追踪。

---

## 社区版层级——Prometheus

### Feature flag

`addon-observability` 已包含在 `default` feature 集中，因此标准社区版构建无需额外 flag。

```sh
cargo build                          # includes addon-observability
cargo build --no-default-features    # no observability
```

### 配置（`config.toml`）

```toml
[metrics]
# Set to false to completely disable both /metrics and /healthz.
enabled      = true

# HTTP path served by GET /metrics (Prometheus text format).
path         = "/metrics"

# HTTP path served by GET /healthz (JSON liveness probe).
healthz_path = "/healthz"

# Optional bearer token.  When set, /metrics requires:
#   Authorization: Bearer <token>
# /healthz is always unauthenticated.
# token = "change-me"
```

所有键都是可选的；未提供该配置节时，采用上面展示的默认值。

### 端点

#### `GET /healthz`

无需认证的存活探针。只要进程仍在运行，就始终返回 HTTP 200。

```json
{
  "status": "ok",
  "uptime_seconds": 3742,
  "version": "0.3.18",
  "active_calls": 4
}
```

#### `GET /metrics`

Prometheus 文本格式的抓取端点。配置了 `token` 时，请求必须携带：

```
Authorization: Bearer <token>
```

Token 缺失或不正确时返回 **HTTP 401**，并带有 `WWW-Authenticate: Bearer realm="metrics"` 响应头。

### 指标参考

下面按类别列出 RustPBX 发出的全部指标：

#### 系统与构建信息

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_info` | Gauge | `version` | 始终为 1；通过标签携带构建版本 |
| `rustpbx_process_uptime_seconds` | Gauge | - | 进程运行时长（秒） |
| `rustpbx_process_resident_memory_bytes` | Gauge | - | 进程常驻内存（字节） |
| `rustpbx_process_open_fds` | Gauge | - | 打开的文件描述符数量 |
| `rustpbx_network_connections` | Gauge | - | 活动网络连接数量 |
| `rustpbx_websocket_connections_total` | Counter | - | 已建立的 WebSocket 连接总数 |
| `rustpbx_websocket_disconnections_total` | Counter | - | WebSocket 断开总数 |
| `rustpbx_websocket_connections_active` | Gauge | - | 当前活动 WebSocket 连接数 |

#### SIP 层

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_sip_registrations_total` | Counter | `realm` | 收到的 REGISTER 请求总数 |
| `rustpbx_sip_registrations_succeeded_total` | Counter | `realm` | 注册成功数 |
| `rustpbx_sip_registrations_failed_total` | Counter | `realm`, `reason` | 注册失败数 |
| `rustpbx_sip_unregistrations_total` | Counter | `realm` | 显式注销数（expires=0） |
| `rustpbx_sip_registrations_active` | Gauge | - | 当前已注册端点数 |
| `rustpbx_sip_dialogs_created_total` | Counter | `direction` | 已创建的 SIP 对话数 |
| `rustpbx_sip_dialogs_terminated_total` | Counter | `direction`, `reason` | 已终止的 SIP 对话数 |
| `rustpbx_sip_dialogs_active` | Gauge | - | 当前活动 SIP 对话数 |
| `rustpbx_sip_responses_total` | Counter | `status_class`, `status_code`, `method` | 已发送的 SIP 响应码数 |
| `rustpbx_sip_invite_latency_seconds` | Histogram | `direction` | INVITE 建立时延 |

#### 通话指标

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_calls_total` | Counter | `direction`, `result` | 已结束通话总数 |
| `rustpbx_call_duration_seconds` | Histogram | `direction` | 从 INVITE 到 BYE 的实际经过时间 |
| `rustpbx_call_talk_time_seconds` | Histogram | `direction` | 通话时间（仅统计已接听通话） |

#### 中继指标

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_trunk_calls_total` | Counter | `trunk_id`, `direction` | 经中继路由的通话数 |
| `rustpbx_trunk_calls_failed_total` | Counter | `trunk_id`, `direction`, `reason` | 中继通话失败数 |
| `rustpbx_trunk_latency_seconds` | Histogram | `trunk_id` | 中继呼叫建立时延 |
| `rustpbx_trunk_status` | Gauge | `trunk_id` | 中继在线状态（1=在线，0=离线） |

#### 媒体（RTP/WebRTC）

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_rtp_packets_sent_total` | Counter | `codec` | 已发送的 RTP 包数 |
| `rustpbx_rtp_packets_received_total` | Counter | `codec` | 已接收的 RTP 包数 |
| `rustpbx_rtp_packets_lost_total` | Counter | `direction` | 丢失的 RTP 包数 |
| `rustpbx_rtp_jitter_seconds` | Histogram | `direction` | RTP 抖动 |
| `rustpbx_media_codec_usage` | Gauge | `codec` | 每种编解码器的当前通话数 |
| `rustpbx_webrtc_connections_total` | Counter | - | 已建立的 WebRTC 连接数 |
| `rustpbx_webrtc_connections_failed_total` | Counter | `reason` | WebRTC 连接失败数 |
| `rustpbx_webrtc_ice_connection_seconds` | Histogram | - | ICE 连接建立时间 |

#### 语音信箱

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_voicemail_messages_total` | Counter | `mailbox` | 收到的语音留言数 |
| `rustpbx_voicemail_duration_seconds` | Histogram | `mailbox` | 语音留言录制时长 |
| `rustpbx_voicemail_messages_stored` | Gauge | `mailbox` | 每个邮箱保存的语音留言数 |

#### 队列

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_queue_size` | Gauge | `queue` | 当前队列长度 |
| `rustpbx_queue_wait_time_seconds` | Histogram | `queue` | 队列等待时间 |
| `rustpbx_queue_abandoned_total` | Counter | `queue` | 放弃排队的呼叫者数 |
| `rustpbx_queue_answered_total` | Counter | `queue` | 从队列接听的呼叫者数 |

#### 转写

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_transcription_requests_total` | Counter | `language` | 转写请求数 |
| `rustpbx_transcription_success_total` | Counter | `language` | 转写成功数 |
| `rustpbx_transcription_failed_total` | Counter | `language`, `reason` | 转写失败数 |
| `rustpbx_transcription_latency_seconds` | Histogram | `language` | 转写处理时间 |
| `rustpbx_transcription_audio_seconds` | Histogram | `language` | 已转写的音频时长 |

#### 路由

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_routing_evaluations_total` | Counter | `direction`, `matched` | 路由评估次数 |
| `rustpbx_routing_default_route_total` | Counter | `direction` | 默认路由使用次数 |
| `rustpbx_routing_evaluation_seconds` | Histogram | - | 路由评估时延 |

#### 身份认证

| 指标 | 类型 | 标签 | 说明 |
|---|---|---|---|
| `rustpbx_auth_attempts_total` | Counter | `method` | 身份认证尝试次数 |
| `rustpbx_auth_success_total` | Counter | `method` | 身份认证成功次数 |
| `rustpbx_auth_failure_total` | Counter | `method`, `reason` | 身份认证失败次数 |

---

#### 标签取值

**`direction`** 的取值：`inbound`、`outbound`、`internal`

**`result`** 的取值：

| 值 | SIP 状态码范围 |
|---|---|
| `ok` | 2xx |
| `redirect` | 3xx |
| `rejected` | 4xx |
| `failed` | 5xx |

### 直方图桶

桶已按电话业务负载预先配置（单位为秒）：

```
0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0
```

### Prometheus 抓取配置示例

```yaml
scrape_configs:
  - job_name: rustpbx
    static_configs:
      - targets: ['pbx-host:8080']
    # Uncomment if token is set:
    # bearer_token: change-me
```

---

## 商业版层级——OpenTelemetry

> 需要 `addon-telemetry` feature（已包含在 `commerce` feature 集中）。

```sh
cargo build --features commerce
```

商业版 addon 会**取代**社区版 addon。它安装同样的 Prometheus 端点，并增加通过 OTLP/gRPC 导出 OpenTelemetry trace 的能力。

### 配置（`config.toml`）

```toml
[metrics]
enabled      = true
path         = "/metrics"
healthz_path = "/healthz"
# token = "change-me"

[otel]
# OTLP gRPC endpoint of your collector (Jaeger / Grafana Tempo / etc.)
endpoint     = "http://otelcol:4317"

# service.name resource attribute sent with every span.
service_name = "rustpbx"

# Head-sampling ratio in [0.0, 1.0].  Default: 0.1 (10 %).
sample_ratio = 0.1

# Also push metrics via OTLP (in addition to Prometheus pull).
export_metrics = false

# Attach trace IDs to structured log records.
log_trace_id = true
```

### 追踪是如何注入的

商业版 addon 使用 `tracing_subscriber::reload`，在 SDK 完成初始化后，将空操作占位层热替换为真正的 OpenTelemetry 层。这样可以避免“必须先有 subscriber，而 OTel SDK 又尚未就绪”的先有鸡还是先有蛋问题。

```
main()
  └─ observability::init_reload_layer()   ← placeholder layer registered first
       └─ tracing_subscriber::registry()
            .with(reload_layer)
            .with(env_filter)
            .with(fmt_layer)
            .try_init()

TelemetryAddon::initialize()
  └─ init OTel SDK + OTLP exporter
  └─ observability::install_otel_layer()  ← hot-swaps placeholder with OTel layer
```

### 每次通话发出的 Span 事件

每次通话结束时，商业版 hook 都会发出结构化的 `tracing::info!` 事件；OTel 层会拾取该事件并将其转换为 OTLP span 事件：

| 字段 | 说明 |
|---|---|
| `call_id` | 唯一通话标识符 |
| `direction` | `inbound` / `outbound` / `internal` |
| `result` | `ok` / `redirect` / `rejected` / `failed` |
| `duration_s` | 通话总时长（秒，浮点数） |
| `talk_s` | 已接听通话的通话时间 |
| `status_code` | 最终 SIP 响应码 |

---

## 单元测试

`addon-observability` 模块附带 **16 个单元测试**，覆盖：

| 测试组 | 测试 |
|---|---|
| Addon 元数据 | `test_addon_id`, `test_addon_category_is_community`, `test_addon_cost_is_free`, `test_addon_name_and_description_nonempty` |
| Recorder 生命周期 | `test_install_recorder_idempotent` |
| `MetricsCallRecordHook` | `test_hook_inbound_answered_ok`, `test_hook_outbound_unanswered_ok`, `test_hook_result_rejected_on_4xx`, `test_hook_result_failed_on_5xx`, `test_hook_result_redirect_on_3xx`, `test_hook_zero_duration_does_not_panic` |
| Auth 中间件 | `test_auth_no_token_configured_allows_all`, `test_auth_valid_bearer_passes`, `test_auth_wrong_bearer_rejected`, `test_auth_missing_header_when_token_required`, `test_auth_empty_bearer_rejected` |

运行命令：

```sh
cargo test --features addon-observability addons::observability::tests
```

---

## Grafana 仪表板（快速开始）

导入随项目提供的仪表板 JSON（即将推出），或使用上面的指标手动添加面板。下面是通话速率的 PromQL 示例：

```promql
# Calls per minute, by direction and result
rate(rustpbx_calls_total[1m])

# P95 call duration
histogram_quantile(0.95, rate(rustpbx_call_duration_seconds_bucket[5m]))

# P95 talk time for answered calls
histogram_quantile(0.95, rate(rustpbx_call_talk_time_seconds_bucket[5m]))
```

## 持续保存媒体处理阶段证据

`[media.quality_stats]` 同时控制媒体腿的 `media.packet_stages` INFO 日志。它从媒体腿创建开始工作，覆盖 IVR、排队和桥接，停止时输出一次标记为 `event="final"`、`snapshot_scope="leg_stopping"` 的停止时快照，保留到现有 `media_quality` 的 `packetDiagnostics`。晚到的实际 deferred 发送结果另见完整关联的 `media.packet_send_tail` INFO，不会回填 CDR。不增加数据库字段，不改变媒体、超时、录音或坐席状态处理。

```toml
[media.quality_stats]
enabled = true
sample_interval_ms = 500
window_ms = 2000
log_interval_ms = 5000
```

采样只读取计数和安全状态，正常按 `log_interval_ms` 输出；初始、首次错误、停流、恢复和选路变化可以提前记录。持续错误合并为一个 burst，选路变化限频且累计，异常记录同时展开当前桥接另一腿。无包只表示相应阶段未增长，DTX、保持、握手和呼叫结束都必须结合上下文分析，不能据此直接判定网络故障。`quality_stats.enabled=false` 关闭 PBX 的采样任务和这些日志；依赖底层的原子计数仍工作，并非零开销开关。未做生产并发压测，不承诺零性能影响。

每行包含完整本节点 `session_id`、`root_session_id`、`leg_id`、方向、单调窗口、累计计数及增量。首个窗口或计数重置后增量为未知，不能把此前 IVR 的累计数解释为桥接后丢包。生产日志时间使用带时区的 RFC3339；`observed_unix_ms` 是 UTC Unix 毫秒，阶段 `first_age_ms` / `last_age_ms` 使用单调时钟。`null` 表示从未观察到，省略的错误阶段表示从未发生、累计为零。快照各字段并非同一原子事务，不应对并发采样的相邻阶段做逐包等式判断。

| 字段/阶段 | 实际累加边界及限制 |
|---|---|
| `stages.ice.udp_read` | 应用成功从专用 UDP socket 读取；含控制包，非网卡或纯 RTP 计数 |
| `shared_udp_read` | 共享 socket 分流后交付，不能冒充原始 socket 读取 |
| `turn_channel_data` / `turn_data_indication` | TURN 解封装成功；未知 channel、格式错误单独记录 |
| `transports[].conn.received` | 解封装后、DTLS/RTP 分类前；含 DTLS 与 RTCP |
| `rtp_accepted` | RTP 解析及需要的 SRTP 认证解密成功后；此前错误/无 session 分开计数 |
| `srtcp_*` / `rtcp_*` | SRTCP 解密、RTCP 解析及 listener 路径；不混入 RTP 接受数 |
| `pipeline_ingress` / `pipeline_egress` | 既有 plaintext tap；egress 在加密、提交前，不能作为发送完成凭据。其增长时间是采样观察时间，不是真正包边界时间 |
| `listener_*` / `bridge_*` | listener 交付、队列满/关闭、未匹配以及 rewrite 桥接提交结果 |
| `send_attempts` / `send_sync_success` | 发送接口尝试与同步返回成功；后者不能证明对端收到 |
| `send_deferred_queued` / `send_async_completed` | 异步入队与后台实际发送接口完成，二者不能混用 |
| `send_errors` / `send_queue_closed` | 真实失败；不修改重试、队列或原有发送返回值 |

字节口径按边界区分：UDP 是读取到的完整 datagram；TURN 解包成功是交付的内层数据；`conn.received` 是交付给分类器的数据；RTP 接受、SRTP 错误和 RTP listener 计数附带原始输入长度（可能仍含认证标签），RTCP 解析/接受计数使用解密后长度；桥接提交及发送成功使用实际提交/返回的长度。不要跨封装、加密前后直接相减字节数并称为丢包。

DTLS 状态只使用不含 keying material 的展示值；现有依赖无法独立提供握手错误分类时明确标记不支持。没有真实重放拒绝分类的版本不伪造重放计数。TURN 控制成功不代表媒体恢复；恢复必须查看对应媒体阶段再次增长。直接转发没有 PCM 音量测量时保持“未测量”，不能填 0。

### 事后关联和查询

1. 从话单或 SIP 日志取得主会话、本节点会话及真正 SIP Call-ID，不能按号码或 ID 前缀猜测关联。
2. 先 `GET /settings/logs/nodes` 枚举实际节点；逐节点请求 `/settings/logs/search`，带明确时区的 `start` / `end`、完整 `callIds` 和 `node`。逐页沿 `nextCursor` 读取到结束。检查 `complete`、`gaps`、`files`，空页也可能有下一页。
3. 查找 `media.packet_stages`，按 `root_session_id` → 本节点 `session_id` → `leg_id` 串联。比较 socket、TURN 解包、认证解密、接受、桥接、发送完成的累计和窗口增量。
4. 读取选中的 local/remote 地址和候选类型。TURN relay 地址与 TURN 日志的 `relayed_addr` 关联，再用真实 `allocation_id` / `client_addr` / `peer_addr` / `channel` 串起两个方向。TURN 不知道 SIP ID，不能伪造；地址需同时考虑时间和端口复用。
5. 从客户端已保存、自动上传的 telemetry 按真实通话 ID、坐席、版本和时间范围取证；比较客户端 socket/解封装/解密、解码、PCM 队列和实际设备边界。录音有声音只能证明录音路径拿到数据，不能证明对端播放。
6. 缺少哪一段就报告该段未知。PBX socket 为零仍不能证明 SBC 没发，发送接口成功仍不能证明 TURN/客户端收到或设备播放。

停止时的 `final` 是 `snapshot_scope="leg_stopping"`，在既有停止发送和关闭 PC 之前取得，不是异步发送 drain 完成凭据。`deferred_sends_pending` 包含正在执行的 deferred write；`deferred_sends_drained_at_snapshot` 只描述该次采样时是否为零。CDR 的 `async_send_results_in_cdr_final=false` 明确说明其不会回填停止之后的结果。晚到的真实发送完成或错误通过 `media.packet_send_tail` INFO 输出，保留完整根/本节点会话、媒体腿和 `pbx_to_peer` 方向；沿同样的完整 ID 查询。首次结果和首次错误立即输出，其余合并到配置日志间隔的一次按需 flush，每个真实连接分别保留最后累计结果。`connection_id` 是同一 `IceConn` 观测 owner 的进程内稳定编号，用于将 CDR/阶段快照与晚到发送日志对应，不是路由、代次或会话 ID；`snapshot_age_ms` 表示日志输出距实际结果采样的时间。`deferred_drained_at_snapshot` 仅表示该次采样已无待处理写入，不表示对端收到或媒体恢复。不会等待队列、改变挂断流程或延长媒体对象生命周期；进程/运行时终止前尚无实际结果的写入仍属未知。

合法协商方向抑制保留为 `send_direction_dropped` / `bridge_direction_dropped`，不计入阶段错误；`bridge_dropped` 保留实际 SRTP 保护或 session 缺失造成的丢弃。RTCP listener 关闭会进入首次错误检测并采集另一腿快照。

`callIds` 的实际格式是 JSON 数组字符串，例如下面的查询参数对象；复用已有管理客户端的鉴权，不在脚本或日志中打印凭据。`node` 必须替换为节点枚举返回的原值，下一页保持同一组参数并加上响应的 `nextCursor` 作为 `cursor`。

```json
{
  "node": "node-value-returned-by-nodes",
  "start": "2026-10-27T16:39:20+08:00",
  "end": "2026-10-27T16:40:30+08:00",
  "callIds": "[\"full-root-session\",\"full-node-session\",\"full-sip-call-id@example.invalid\"]",
  "limit": 200
}
```

客户端历史上报先通过现有 `/client-reports/list` 按 `from` / `to`、`agent_id`、`version`、`event` 定位报告，使用 `before_id` 翻页；再用 `/client-reports/body?id=...&offset=0&limit=262144` 沿 `next_offset` 读取到末尾，核对 `total_bytes`、`truncated`、`received_by` 和完整性。不同部署 API 前缀及权限沿用已有管理客户端。取到上报摘要不等于取到全天日志，正文缺失或接收节点不可达必须明确说明。

历史查询优先选择目标时间的归档，再扫描可能包含延迟事件的文件。候选数量、扫描预算或轮转变化造成不完整时会返回 `gaps`，不得将 `complete=false` 的结果作为完整取证。当前活动文件和小时/日归档（含碰撞编号）均须保留。

### 发布和保留

配置应通过既有发布流程统一下发到所有 call、agent 和 TURN 节点；不得通过实例管理 API 临时改日志级别。仓库配置只能证明配置源，不能证明线上版本、集群选择和运行时配置已经生效。

PBX 当前使用异步文件写入和小时/日轮转；本实现不负责自动删除旧归档。部署平台必须将归档与服务工作目录一起持久保存，并按报障窗口设置容量和清理策略，建议至少 30 天。日志丢失、磁盘满、收集丢弃或上传失败均属于证据缺口，应在发布后的运行核验中确认；本地测试不能证明线上保留条件。客户端自动上传必须保持现有 outbox、重试及回执策略，人工补采只是补充。

### 本次实现的验证与边界

PBX 定向回归使用 `cargo test-fast --offline --locked -- packet_diagnostics recording_disabled_still_arms_capture_and_allows_on_demand_start test_record_snapshot_carries_transferred_and_leg_timeline cleanup_keeps_session_live_for_watchdog_until_ended_hooks_finish --nocapture`，8 项通过。包括独立多线程媒体运行时的实际 INFO 输出、完整 ID 历史查询、桥接前和终态快照、禁用配置、真实 UDP 下的解析失败，以及真实 SDES-SRTP 认证失败后有效媒体恢复；复用了录音与会话清理回归。

依赖定向验证使用 `cargo test --offline --locked -p rustrtc --lib -- packet_diagnostics test_try_send_defers_on_non_udp_transport fast_path_try_send_relays_rtp_via_turn`，8 项通过。成功解包不包含未知 channel，控制历史覆盖真实认证、刷新、超时与取消；TCP 入队和写出完成分别验证。

TURN 使用 `cargo test --offline --lib --test hairpin_e2e`，216 项库测试、2 项双中继集成测试通过。软电话的真实加密媒体 4 项、依赖 RTP 26 项、TURN 2 项、PC 状态 78 项、telemetry 13 项、设备 18 项、实际 VPIO callback 合成缓冲 1 项通过；完整命令及结果随改动交付。

客户端日文件当前保留 7 天；停流/恢复通过既有持久 outbox 自动上传，并在首次发送前固化对应通话日志切片，失败重试和重启恢复继续复用原机制。服务端上报正文与元数据沿用既有约 30 天清理流程。发布时必须确认目标版本、自动上报开关、接收地址、双存储成功回执，以及本地文件和 outbox 所在目录的持久性。不能用代码默认开启来证明某台运行中的客户端已开启。

客户端可信根会话来自 purpose-qualified `User-to-User` 的既有解析；没有可信头时为 `None`，PBX 本节点 ID 未通过该客户端入口上报。需要用完整 SIP Call-ID 和 PBX SIP 日志建立节点映射。设备 callback/PCM 消费计数明确属于进程全局并携带 generation，不能把混音、铃声或其他通话的设备计数当作单通话播放凭据。

未执行真实麦克风/扬声器端到端可听验收、Windows/iOS 编译运行、no_std 验证、生产并发压测或真实线上 collector 回执核验。VPIO 测试验证实际回调向合成 OS 缓冲复制 PCM 与补零，不等于硬件可听。日志和计数增强不证明任何历史工单的断音根因已被修复。

### 对抗性审查后的修正

已补齐咨询腿实际 manager 的独立采样与自动持久上报；诊断窗口使用真实 transport accepted 累计，避免多 SSRC 的业务统计覆盖造成停流误报。PCM 提交尝试、入队、满/关闭及实际编码消费分别记录；编码、解码、设备边界已有真实 first/last 活动时间，设备计数仍明确属于进程全局。

PBX 的 `final` 是 `leg_stopping` 快照，异步发送晚到完成/失败由 `media.packet_send_tail` 按完整会话与连接 ID 留存，不能把停止时快照称作已发送完毕。正常方向抑制不再计为阶段错误，RTCP listener 关闭触发异常取证。TURN 的删除/过期 teardown 与真正 drain final 分开，挂起 TCP 写入不再阻止删除时证据输出；畸形 Send 和 orphan 拒绝的字节及首末时间已补齐。客户端认证成功后的 RTCP parse 失败也有独立计数。

本轮修复验证：PBX 8、media 2、PBX rustrtc 8；TURN 6 个不同受影响测试；客户端 157 个相关不同测试。包含真实咨询 RTC/SIP → EmbeddedEngine → wire → Brain → outbox 的停流和恢复，已核对报告保存实际咨询 Call-ID/root 及详细日志切片。未改媒体控制或业务状态处理，未提交或部署；真实硬件、Windows/iOS、生产压力及部署接收端回执仍未验证。
