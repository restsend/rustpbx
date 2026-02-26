# Observability

RustPBX supports two tiers of observability, controlled by Cargo feature flags:

| Tier | Feature flag | What you get |
|---|---|---|
| Community | `addon-observability` (default) | Prometheus scrape endpoint + liveness probe |
| Commercial | `addon-telemetry` (replaces community) | Prometheus + OpenTelemetry traces (OTLP/gRPC) |

The two tiers are mutually exclusive at compile time.  
When `addon-telemetry` is active it installs the same Prometheus recorder and endpoints as the community tier, **plus** full distributed tracing.

---

## Community tier — Prometheus

### Feature flag

`addon-observability` is included in the `default` feature set, so no extra flags are required for a standard community build.

```sh
cargo build                          # includes addon-observability
cargo build --no-default-features    # no observability
```

### Configuration (`config.toml`)

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

All keys are optional; the defaults shown above apply when the section is absent.

### Endpoints

#### `GET /healthz`

Unauthenticated liveness probe.  Always returns HTTP 200 while the process is running.

```json
{
  "status": "ok",
  "uptime_seconds": 3742,
  "version": "0.3.18",
  "active_calls": 4
}
```

#### `GET /metrics`

Prometheus text-format scrape endpoint.  When `token` is configured, the request must carry:

```
Authorization: Bearer <token>
```

Missing or incorrect tokens receive **HTTP 401** with a `WWW-Authenticate: Bearer realm="metrics"` header.

### Metrics reference

All metrics emitted by RustPBX, organized by category:

#### System & Build Info

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_info` | Gauge | `version` | Always 1; carries the build version label |
| `rustpbx_process_uptime_seconds` | Gauge | - | Process uptime in seconds |
| `rustpbx_process_resident_memory_bytes` | Gauge | - | Process resident memory in bytes |
| `rustpbx_process_open_fds` | Gauge | - | Number of open file descriptors |
| `rustpbx_network_connections` | Gauge | - | Number of active network connections |
| `rustpbx_websocket_connections_total` | Counter | - | Total WebSocket connections established |
| `rustpbx_websocket_disconnections_total` | Counter | - | Total WebSocket disconnections |
| `rustpbx_websocket_connections_active` | Gauge | - | Current active WebSocket connections |

#### SIP Layer

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_sip_registrations_total` | Counter | `realm` | Total REGISTER requests received |
| `rustpbx_sip_registrations_succeeded_total` | Counter | `realm` | Successful registrations |
| `rustpbx_sip_registrations_failed_total` | Counter | `realm`, `reason` | Failed registrations |
| `rustpbx_sip_unregistrations_total` | Counter | `realm` | Explicit unregistrations (expires=0) |
| `rustpbx_sip_registrations_active` | Gauge | - | Current number of registered endpoints |
| `rustpbx_sip_dialogs_created_total` | Counter | `direction` | SIP dialogs created |
| `rustpbx_sip_dialogs_terminated_total` | Counter | `direction`, `reason` | SIP dialogs terminated |
| `rustpbx_sip_dialogs_active` | Gauge | - | Current active SIP dialogs |
| `rustpbx_sip_responses_total` | Counter | `status_class`, `status_code`, `method` | SIP response codes sent |
| `rustpbx_sip_invite_latency_seconds` | Histogram | `direction` | INVITE setup latency |

#### Call Metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_calls_total` | Counter | `direction`, `result` | Total completed calls |
| `rustpbx_call_duration_seconds` | Histogram | `direction` | Wall-clock time from INVITE to BYE |
| `rustpbx_call_talk_time_seconds` | Histogram | `direction` | Talk time (only for answered calls) |

#### Trunk Metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_trunk_calls_total` | Counter | `trunk_id`, `direction` | Calls routed through trunk |
| `rustpbx_trunk_calls_failed_total` | Counter | `trunk_id`, `direction`, `reason` | Failed trunk calls |
| `rustpbx_trunk_latency_seconds` | Histogram | `trunk_id` | Trunk call setup latency |
| `rustpbx_trunk_status` | Gauge | `trunk_id` | Trunk online status (1=online, 0=offline) |

#### Media (RTP/WebRTC)

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_rtp_packets_sent_total` | Counter | `codec` | RTP packets sent |
| `rustpbx_rtp_packets_received_total` | Counter | `codec` | RTP packets received |
| `rustpbx_rtp_packets_lost_total` | Counter | `direction` | RTP packets lost |
| `rustpbx_rtp_jitter_seconds` | Histogram | `direction` | RTP jitter |
| `rustpbx_media_codec_usage` | Gauge | `codec` | Current calls per codec |
| `rustpbx_webrtc_connections_total` | Counter | - | WebRTC connections established |
| `rustpbx_webrtc_connections_failed_total` | Counter | `reason` | WebRTC connection failures |
| `rustpbx_webrtc_ice_connection_seconds` | Histogram | - | ICE connection establishment time |

#### Voicemail

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_voicemail_messages_total` | Counter | `mailbox` | Voicemail messages received |
| `rustpbx_voicemail_duration_seconds` | Histogram | `mailbox` | Voicemail recording duration |
| `rustpbx_voicemail_messages_stored` | Gauge | `mailbox` | Voicemails stored per mailbox |

#### Queue

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_queue_size` | Gauge | `queue` | Current queue size |
| `rustpbx_queue_wait_time_seconds` | Histogram | `queue` | Queue wait time |
| `rustpbx_queue_abandoned_total` | Counter | `queue` | Callers who abandoned queue |
| `rustpbx_queue_answered_total` | Counter | `queue` | Callers answered from queue |

#### Transcription

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_transcription_requests_total` | Counter | `language` | Transcription requests |
| `rustpbx_transcription_success_total` | Counter | `language` | Successful transcriptions |
| `rustpbx_transcription_failed_total` | Counter | `language`, `reason` | Failed transcriptions |
| `rustpbx_transcription_latency_seconds` | Histogram | `language` | Transcription processing time |
| `rustpbx_transcription_audio_seconds` | Histogram | `language` | Audio duration transcribed |

#### Routing

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_routing_evaluations_total` | Counter | `direction`, `matched` | Route evaluations |
| `rustpbx_routing_default_route_total` | Counter | `direction` | Default route usage |
| `rustpbx_routing_evaluation_seconds` | Histogram | - | Route evaluation latency |

#### Authentication

| Metric | Type | Labels | Description |
|---|---|---|---|
| `rustpbx_auth_attempts_total` | Counter | `method` | Authentication attempts |
| `rustpbx_auth_success_total` | Counter | `method` | Successful authentications |
| `rustpbx_auth_failure_total` | Counter | `method`, `reason` | Failed authentications |

---

#### Label Values

**`direction`** values: `inbound`, `outbound`, `internal`

**`result`** values:

| Value | SIP status range |
|---|---|
| `ok` | 2xx |
| `redirect` | 3xx |
| `rejected` | 4xx |
| `failed` | 5xx |

### Histogram buckets

Buckets are pre-configured for telephony workloads (values in seconds):

```
0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0
```

### Prometheus scrape config example

```yaml
scrape_configs:
  - job_name: rustpbx
    static_configs:
      - targets: ['pbx-host:8080']
    # Uncomment if token is set:
    # bearer_token: change-me
```

---

## Commercial tier — OpenTelemetry

> Requires the `addon-telemetry` feature (bundled in the `commerce` feature set).

```sh
cargo build --features commerce
```

The commercial addon **supersedes** the community addon.  It installs the same
Prometheus endpoint and adds OpenTelemetry trace export via OTLP/gRPC.

### Configuration (`config.toml`)

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

### How tracing is injected

The commercial addon uses `tracing_subscriber::reload` to hot-swap a no-op
placeholder with a live OpenTelemetry layer after the SDK is fully initialised.
This avoids the chicken-and-egg problem of needing the subscriber to exist
before the OTel SDK is ready.

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

### Span events emitted per call

On every completed call the commercial hook emits a structured `tracing::info!`
event that the OTel layer picks up and turns into an OTLP span event:

| Field | Description |
|---|---|
| `call_id` | Unique call identifier |
| `direction` | `inbound` / `outbound` / `internal` |
| `result` | `ok` / `redirect` / `rejected` / `failed` |
| `duration_s` | Total call duration (seconds, float) |
| `talk_s` | Talk time for answered calls |
| `status_code` | Final SIP response code |

---

## Unit tests

The `addon-observability` module ships with **16 unit tests** covering:

| Test group | Tests |
|---|---|
| Addon metadata | `test_addon_id`, `test_addon_category_is_community`, `test_addon_cost_is_free`, `test_addon_name_and_description_nonempty` |
| Recorder lifecycle | `test_install_recorder_idempotent` |
| `MetricsCallRecordHook` | `test_hook_inbound_answered_ok`, `test_hook_outbound_unanswered_ok`, `test_hook_result_rejected_on_4xx`, `test_hook_result_failed_on_5xx`, `test_hook_result_redirect_on_3xx`, `test_hook_zero_duration_does_not_panic` |
| Auth middleware | `test_auth_no_token_configured_allows_all`, `test_auth_valid_bearer_passes`, `test_auth_wrong_bearer_rejected`, `test_auth_missing_header_when_token_required`, `test_auth_empty_bearer_rejected` |

Run them with:

```sh
cargo test --features addon-observability addons::observability::tests
```

---

## Grafana dashboard (quick-start)

Import the bundled dashboard JSON (coming soon) or add a panel manually using the metrics above.  Example PromQL for call rate:

```promql
# Calls per minute, by direction and result
rate(rustpbx_calls_total[1m])

# P95 call duration
histogram_quantile(0.95, rate(rustpbx_call_duration_seconds_bucket[5m]))

# P95 talk time for answered calls
histogram_quantile(0.95, rate(rustpbx_call_talk_time_seconds_bucket[5m]))
```
