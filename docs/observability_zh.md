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
