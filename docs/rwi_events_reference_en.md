# RWI Events Developer Reference

> Source code: `src/rwi/proto.rs` | Protocol version: `1.0`

---

## 1. Overview

RustPBX streams real-time call, IVR, recording, queue, agent, and extension events through the RWI (Real-time WebSocket Interface). Developers can receive events via two channels:

| Channel | Protocol | Use Case |
|---------|----------|----------|
| **WebSocket subscription** | `ws(s)://<host>/rwi/v1` | Real-time bidirectional interaction (bots, softphones, dashboards) |
| **Webhook callback** | HTTP POST | Async notifications (CRM, recording systems, analytics) |

### Dispatch Methods

| Method | Recipient | Meaning |
|--------|-----------|---------|
| `call_owner` | WS session owning the call_id | Per-call fine-grained events |
| `fan_out` | All WS sessions subscribed to the context | Incoming call notifications, IVR events |
| `broadcast` | All online WS sessions | Global events (agent state, DN registration, etc.) |
| `webhook` | Configured HTTP endpoint | All events forwarded (filterable) |

---

## 2. Connection & Authentication

### WebSocket

```
GET /rwi/v1 HTTP/1.1
Upgrade: websocket
Authorization: Bearer <token>
```

Or via query parameter: `GET /rwi/v1?token=<token>`

### Webhook Configuration (rustpbx.toml)

```toml
[rwi_webhook]
url = "https://myapp.example.com/rwi-events"
timeout_ms = 5000
headers = { Authorization = "Bearer your-token" }
events = ["call_hangup", "record_stopped", "dn_state_changed"]   # empty = all events
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | String | (required) | HTTP endpoint receiving POST requests |
| `timeout_ms` | u64 | 5000 | HTTP request timeout in milliseconds |
| `headers` | HashMap | (optional) | Custom HTTP headers sent with every request |
| `events` | Vec\<String\> | [] (all) | Event type whitelist; empty forwards all events |

---

## 3. Envelope Format

### WebSocket Event

```json
{
  /* Event fields are flattened directly at the top level, no extra wrapping */
}
```

Example:
```json
{
  "call_id": "call-abc123",
  "caller_name": "330909",
  "callee_name": "9242000001",
  "direction": "inbound"
}
```

> WebSocket events are sent with the event fields directly as top-level JSON keys, without a `"rwi"` or event-type-name wrapper. Clients identify events through the subscription rules negotiated at connection time.

### Webhook Envelope

```json
{
  "rwi": "1.0",
  "sequence": 42,
  "timestamp": 1716212345,
  "call_id": "call-abc123",
  "event_type": "call_ringing",
  "event": {
    /* identical to WS event content (no event_type wrapper) */
  }
}

| Field | Type | Description |
|-------|------|-------------|
| `rwi` | string | Protocol version `"1.0"` |
| `sequence` | u64 | Monotonically increasing event sequence number (for dedup and resume) |
| `timestamp` | u64 | Unix epoch seconds |
| `call_id` | string | Call identifier (empty string for broadcast-only events) |
| `event_type` | string | snake_case event type name |
| `event` | object | Event payload with fields flattened directly (no event_type wrapper) |

---

## 4. Flat Call Context (EventCallContext)

All call-scoped events use `#[serde(flatten)]` to embed the following fields **directly into the event JSON** (no nested object). `None` values are automatically omitted.

| Field | Type | Description |
|-------|------|-------------|
| `caller` | Option\<String\> | Caller SIP URI |
| `callee` | Option\<String\> | Callee SIP URI |
| `caller_name` | Option\<String\> | Calling party number (normalized digits) |
| `callee_name` | Option\<String\> | Dialed number / DNIS |
| `direction` | Option\<String\> | `inbound` / `outbound` / `internal` |
| `trunk` | Option\<String\> | SIP trunk name |
| `app_id` | Option\<String\> | IVR application ID |
| `routing_target` | Option\<String\> | Current routing target |
| `agent_id` | Option\<String\> | Agent ID |
| `agent_name` | Option\<String\> | Agent display name |

**Notes**:
- `ani` vs `caller`: `ani` is a plain number (for business logic), `caller` is the full SIP URI
- `dnis` vs `callee`: same distinction
- Context is injected by `CallMetaStore` at gateway dispatch time — event producers never fill it manually

### Field Overlap Explanation

Some events (e.g., `RecordStopped`, `IvrNodeEntered`) carry their own `ani`/`dnis` fields. When an event's own field is `None`, `enrich()` automatically backfills from context. Webhook consumers always receive the merged result.

---

## 5. Subscription & Session Resume

### Subscribe to Contexts

```json
{
  "rwi": "1.0",
  "action_id": "sub-001",
  "action": "session.subscribe",
  "params": { "contexts": ["queue:support", "agent:*"] }
}
```

| Context Format | Description |
|----------------|-------------|
| `queue:<queue_id>` | Subscribe to queue events |
| `agent:<agent_id>` | Subscribe to agent events |
| `*` | Wildcard — receive all broadcast events |

### Session Resume (Reconnection)

```json
{
  "rwi": "1.0",
  "action_id": "resume-001",
  "action": "session.resume",
  "params": { "last_sequence": 42 }
}
```

Server buffers the latest 1000 events (60-second retention). After reconnection, all events after `last_sequence` are replayed.

### Webhook Deduplication

The webhook handler deduplicates using `(call_id, sequence)` tuples in a 4096-entry ring buffer. Duplicate events are silently dropped.

---

## 6. Complete Event Dictionary

> In the tables below, `+ctx` means the event carries flat context fields.
> `?` indicates an `Option<T>` field — omitted from JSON when `null`.

### 6.1 Call Lifecycle

#### call_incoming

Dispatch: fan_out_to_context

New call enters the system. First event in any call flow.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Unique call identifier |
| `context` | String | Dialplan context |
| `caller` | String | Caller SIP URI |
| `callee` | String | Callee SIP URI |
| `dial_direction` | String | `inbound` / `outbound` / `internal` |
| `trunk` | Option\<String\> | SIP trunk name |
| `sip_headers` | Map\<String, String\> | Whitelisted SIP headers |
| `root_call_id` | Option\<String\> | Root call ID (constant across transfers) |
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number / DNIS |
| `called_phone` | Option\<String\> | Actual called number (outbound scenario) |
| `app_id` | Option\<String\> | IVR application ID |
| `routing_target` | Option\<String\> | Routing target |
| `uuid` | Option\<String\> | Global UUID (for recording linkage) |
| `routing_path` | Option\<Vec\<String\>\> | Routing path sequence |

> **Note**: `call_incoming` uses `dial_direction`; other events' context uses `direction`.

```json
{
  "rwi": "1.0",
  "call_incoming": {
    "call_id": "call-abc",
    "context": "inbound",
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "dial_direction": "inbound",
    "trunk": "trunk_sip",
    "sip_headers": { "X-Tenant": "corp_a" },
    "root_call_id": "call-root-42",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "called_phone": null,
    "app_id": "ivr_sales",
    "routing_target": "queue:support",
    "uuid": "uuid-abc-123",
    "routing_path": ["menu:root", "queue:level1"]
  }
}
```

#### call_ringing / call_early_media / call_answered / call_unbridged / call_no_answer / call_busy

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| *+ctx* | | Flat context fields |

```json
{
  "rwi": "1.0",
  "call_ringing": {
    "call_id": "call-abc",
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "direction": "inbound"
  }
}
```

#### call_bridged

Dispatch: call_owner (both legs receive it)

| Field | Type | Description |
|-------|------|-------------|
| `leg_a` | String | A-leg call_id |
| `leg_b` | String | B-leg call_id |

#### call_hangup

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `reason` | Option\<String\> | Hangup reason (see table below) |
| `sip_status` | Option\<u16\> | SIP response code |
| *+ctx* | | Flat context fields |

**reason values**:

| Value | Description |
|-------|-------------|
| `caller` | Caller hung up |
| `callee` | Callee hung up |
| `refer` | REFER transfer hangup |
| `system` | System hangup |
| `autohangup` | Auto hangup (timeout) |
| `noAnswer` | No answer (408/480/487) |
| `rejected` | Rejected/busy (486/600/603) |
| `canceled` | Canceled (487) |
| `failed` | Generic failure (other 4xx) |
| `serverUnavailable` | Server unavailable (5xx) |
| `rtpTimeout` | RTP timeout |

```json
{
  "rwi": "1.0",
  "call_hangup": {
    "call_id": "call-abc",
    "reason": "caller",
    "sip_status": null,
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "direction": "inbound"
  }
}
```

### 6.2 Transfer Events

#### call_transferred / call_transfer_accepted

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| *+ctx* | | Flat context fields |

#### call_transfer_failed

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `sip_status` | Option\<u16\> | SIP status code |
| `reason` | Option\<String\> | Failure reason |
| *+ctx* | | Flat context fields |

### 6.3 Media Events

#### media_hold_started / media_hold_stopped / media_stream_started / media_stream_stopped

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| *+ctx* | | Flat context fields |

#### media_ringback_passthrough_started / media_ringback_passthrough_stopped

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `source` | String | Source leg call_id |
| `target` | String | Target leg call_id |

#### media_play_started / media_play_finished

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `leg_id` | Option\<String\> | Target leg |
| `track_id` | String | Playback track ID |
| `interrupted` | bool | `media_play_finished` only: whether interrupted by DTMF |
| *+ctx* | | Flat context fields |

#### dtmf

Dispatch: fan_out_to_context

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `digit` | String | DTMF digit (`0`-`9`, `*`, `#`) |
| `leg_id` | Option\<String\> | Leg that generated the DTMF |
| *+ctx* | | Flat context fields |

#### dtmf_collected / dtmf_collection_timeout

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `leg_id` | String | Leg that provided the digits |
| `digits` | String | `dtmf_collected` only: collected digit string |
| *+ctx* | | Flat context fields |

---

### 6.4 Recording Events

#### record_started / record_paused / record_resumed / record_failed

Dispatch: call_owner

> Trigger: Via `RecordStart` / `RecordPause` / `RecordResume` / `RecordStop` RWI commands. **Not automatic** — recording does not start automatically when a call is answered.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `error` | String | `record_failed` only: error message |
| *+ctx* | | Flat context fields |

#### record_stopped (Enhanced)

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `duration_secs` | Option\<u64\> | Recording duration in seconds |
| `filename` | Option\<String\> | Recording filename |
| `unique_id` | Option\<String\> | Recording UUID |
| `file_size` | Option\<u64\> | File size in bytes |
| `download_url` | Option\<String\> | Download URL |
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number |
| `called_phone` | Option\<String\> | Actual called number |
| `call_type` | Option\<String\> | `inbound`/`outbound`/`internal`/`consult` |
| `agent_id` | Option\<String\> | Agent ID |
| `agent_name` | Option\<String\> | Agent name |
| `call_start_time` | Option\<String\> | Call start timestamp (ISO 8601) |
| `call_end_time` | Option\<String\> | Call end timestamp |
| `upload_time` | Option\<String\> | Upload completion timestamp |
| `switch_flag` | Option\<String\> | Site identifier (e.g., `ks`, `bj`) |
| `root_call_id` | Option\<String\> | Root call ID |

> Note: `record_stopped` does not carry flat context, but includes its own `ani`/`dnis` fields. `enrich()` backfills `None` fields from context.

```json
{
  "rwi": "1.0",
  "record_stopped": {
    "call_id": "call-abc",
    "duration_secs": 51,
    "filename": "uuid_2026-05-14_08-11-49.mp3",
    "unique_id": "uuid-abc-123",
    "file_size": 149517,
    "download_url": "https://storage.example.com/rec.mp3",
    "caller_name": "330909",
    "callee_name": "9242000001",
    "called_phone": "018659727661",
    "call_type": "outbound",
    "agent_id": "451447",
    "agent_name": "luoxiaofeng90_v",
    "call_start_time": "2026-05-14T08:11:35Z",
    "call_end_time": "2026-05-14T08:12:26Z",
    "upload_time": "2026-05-14T16:14:46Z",
    "switch_flag": "ks",
    "root_call_id": "call-root-42"
  }
}
```

#### recording_metadata_available

Dispatch: call_owner

Triggered when the recording file upload completes, containing full metadata.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `metadata` | RecordingMetadata | Recording metadata (see below) |

**RecordingMetadata fields**:

| Field | Type | Description |
|-------|------|-------------|
| `filename` | String | Recording filename (required) |
| `unique_id` | String | Recording UUID (required) |
| `file_size` | u64 | File size in bytes (required) |
| `download_url` | Option\<String\> | Download URL |
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number |
| `called_phone` | Option\<String\> | Actual called number |
| `call_type` | String | Call type (required) |
| `agent_id` | Option\<String\> | Agent ID |
| `agent_name` | Option\<String\> | Agent name |
| `call_start_time` | Option\<String\> | Call start timestamp |
| `call_end_time` | Option\<String\> | Call end timestamp |
| `upload_time` | Option\<String\> | Upload completion timestamp |
| `switch_flag` | Option\<String\> | Site identifier |
| `process_flag` | Option\<String\> | Process identifier (e.g., `ks_22_normal`) |
| `root_call_id` | Option\<String\> | Root call ID |

```json
{
  "rwi": "1.0",
  "recording_metadata_available": {
    "call_id": "call-abc",
    "metadata": {
      "filename": "uuid_2026-05-14.mp3",
      "unique_id": "uuid-abc-123",
      "file_size": 149517,
      "download_url": "https://storage.example.com/rec.mp3",
      "caller_name": "330909",
      "callee_name": "9242000001",
      "called_phone": null,
      "call_type": "inbound",
      "agent_id": "451447",
      "agent_name": "luoxiaofeng90_v",
      "call_start_time": "2026-05-14T08:11:35Z",
      "call_end_time": "2026-05-14T08:12:26Z",
      "upload_time": "2026-05-14T16:14:46Z",
      "switch_flag": "ks",
      "process_flag": "ks_22_normal",
      "root_call_id": "call-root-42"
    }
  }
}
```

#### record_end

Dispatch: call_owner

Recording finalisation event. Emitted after the recording upload completes; if no upload is configured, it fires when the local recording file is ready (using the local path as url). Also emitted after SipFlow media upload completes.

> **Trigger conditions**:
> - Regular recording: automatically emitted by `RecordingUploadHook` after `CallRecordManager` processes the record
> - SipFlow recording: emitted after SipFlow media file upload to S3/HTTP completes
> - **Not** triggered by the `RecordStop` command — unlike `record_started`/`record_stopped` which require an explicit command

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `url` | Option\<String\> | Upload URL (if uploaded), local file path (no upload), or SipFlow media file URL |
| `duration_secs` | u64 | Recording duration (seconds) |
| `file_size` | u64 | File size (bytes) |

---

### 6.5 IVR Events

All IVR events carry flat context fields.

#### ivr_node_entered

Dispatch: fan_out_to_context

Call enters an IVR node (menu, prompt, etc.).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `node_id` | String | Node ID |
| `node_name` | String | Node name |
| `node_type` | String | Node type (`menu`, `prompt`, `transfer`, etc.) |
| `app_id` | String | IVR application ID |
| `entry_time` | String | Entry timestamp (ISO 8601) |
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number |
| `routing_target` | Option\<String\> | Routing target |
| `previous_node_id` | Option\<String\> | Previous node ID |
| *+ctx* | | Flat context fields |

#### ivr_node_exited

Dispatch: fan_out_to_context

Call exits an IVR node.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `node_id` | String | Node ID |
| `node_name` | String | Node name |
| `result_value` | Option\<String\> | User DTMF or branch result |
| `duration_ms` | u32 | Node dwell time in milliseconds |
| `exit_time` | String | Exit timestamp |
| `next_node_id` | Option\<String\> | Next node ID |
| `hangup_reason` | Option\<String\> | Hangup reason |
| `call_result` | Option\<String\> | Call result |
| *+ctx* | | Flat context fields |

#### ivr_flow_transitioned

Dispatch: fan_out_to_context

Call transitions between IVR applications.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `from_app_id` | String | Source application ID |
| `to_app_id` | String | Target application ID |
| `from_node_id` | String | Source node ID |
| `to_node_id` | String | Target node ID |
| `transition_reason` | String | Transition reason (`menu_choice`, `transfer`, `overflow`, etc.) |
| `transition_time` | String | Transition timestamp |
| `next_routing_target` | Option\<String\> | Next routing target |
| *+ctx* | | Flat context fields |

#### ivr_flow_completed

Dispatch: fan_out_to_context

IVR flow completes (terminal action executed: Transfer, Queue, Voicemail, Hangup).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `app_id` | String | IVR application ID |
| `total_nodes_traversed` | u32 | Total nodes traversed |
| `total_duration_ms` | u32 | Total IVR duration in milliseconds |
| `final_result` | String | Final result (`transferred`, `voicemail`, `abandoned`, etc.) |
| `completion_time` | String | Completion timestamp |
| `final_routing_target` | Option\<String\> | Final routing target |
| *+ctx* | | Flat context fields |

```json
{
  "rwi": "1.0",
  "ivr_flow_completed": {
    "call_id": "call-abc",
    "app_id": "ivr-sales",
    "total_nodes_traversed": 3,
    "total_duration_ms": 15200,
    "final_result": "transferred",
    "completion_time": "2026-05-14T17:55:00Z",
    "final_routing_target": "queue:support",
    "caller": "13800138000",
    "direction": "inbound"
  }
}
```

#### ivr_step_trace

Dispatch: fan_out_to_context

Step-mode IVR trace event. Emitted on each provider round-trip or action execution completion.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `session_id` | String | Session ID |
| `caller` | String | Caller |
| `callee` | String | Callee |
| `step_index` | u32 | Step index |
| `event_type` | String | Event type (e.g., `session_start`, `dtmf`, `audio_complete`, `action_execute`) |
| `action_type` | String | Action type (e.g., `Transfer`, `Prompt`, `DtmfMenu`) |
| `action_json` | Option\<String\> | Action details JSON |
| `result_kind` | String | Result type (`terminal`, `continue`, `error`) |
| `duration_ms` | u64 | Step execution duration (ms), always present |
| `error` | Option\<String\> | Error message |
| `step_id` | Option\<String\> | Current node ID, returned by provider via ActionNode.step_id |
| `step_name` | Option\<String\> | Current node name, returned by provider via ActionNode.step_name |
| `step_start_time` | Option\<String\> | Current step start time (ISO UTC) |
| `step_end_time` | Option\<String\> | Current step end time (ISO UTC). Only present when step execution completes (terminal/error); null during WaitFor (waiting for user input) |
| `extra` | Option\<JSON Object\> | Transparent passthrough data from provider. Provider returns the complete object in ActionNode.extra each time; RustPBX stores and outputs it as-is |

> **Timing fields**:
> - `step_start_time` — when the current step started (previous step end or session start)
> - `step_end_time` — when the step ended (only on completion)
>
> **Duration fields**:
> - `duration_ms` — step execution duration (ms), always present, includes provider round-trip and action execution time

---

### 6.6 Queue / ACD Events

All queue events carry flat context fields.

#### queue_joined

Dispatch: call_owner / broadcast

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| *+ctx* | | Flat context fields |

#### queue_position_changed

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `position` | u32 | Current queue position |
| *+ctx* | | Flat context fields |

#### queue_agent_offered / queue_agent_connected

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `agent_id` | String | Agent ID |
| *+ctx* | | Flat context fields |

#### queue_left

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `reason` | Option\<String\> | Leave reason |
| *+ctx* | | Flat context fields |

#### queue_wait_timeout

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| *+ctx* | | Flat context fields |

#### queue_overflowed

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `original_queue_id` | String | Original queue ID |
| `overflow_queue_id` | String | Overflow target queue ID |
| `reason` | String | Overflow reason |
| *+ctx* | | Flat context fields |

#### queue_voicemail_redirected

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `reason` | String | Reason |
| *+ctx* | | Flat context fields |

#### queue_candidates_found

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `candidates` | Vec\<String\> | Candidate agent list |
| `trace_id` | String | ACD trace ID |
| *+ctx* | | Flat context fields |

#### queue_agent_ringing / queue_agent_no_answer / queue_agent_rejected

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `agent_id` | String | Agent ID |
| `attempt` | u32 | `no_answer`/`rejected` only: attempt number |
| `trace_id` | String | ACD trace ID |
| *+ctx* | | Flat context fields |

#### queue_fallback_executed

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `action` | String | Fallback action executed |
| `reason` | String | Reason |
| `trace_id` | String | ACD trace ID |
| *+ctx* | | Flat context fields |

#### queue_alert

Dispatch: broadcast (no call_id)

| Field | Type | Description |
|-------|------|-------------|
| `queue_id` | String | Queue ID |
| `alert_type` | String | Alert type |
| `message` | String | Alert message |

---

### 6.7 Agent State Events

#### agent_state_changed

Dispatch: broadcast

Agent state machine transition.

| Field | Type | Description |
|-------|------|-------------|
| `agent_id` | String | Agent ID |
| `from_status` | String | Previous status |
| `to_status` | String | New status |
| `call_id` | Option\<String\> | Associated call ID |
| `agent_name` | Option\<String\> | Agent display name |
| `agent_extension` | Option\<String\> | Agent extension number |
| `caller` | Option\<String\> | Caller / directory number |
| `team_id` | Option\<String\> | Team ID |
| `duration_secs` | Option\<u32\> | Duration in previous status |
| `reason_code` | Option\<String\> | Reason code (e.g., `CALL`, `BREAK`, `TRAINING`) |

**Agent status values**:

| Status | Description | Can transition to |
|--------|-------------|-------------------|
| `offline` | Disconnected | `idle`, `away`, `dnd` |
| `idle` | Ready to accept calls | `ringing`, `away`, `dnd`, `offline` |
| `away` | Online but not accepting (break) | `idle`, `dnd`, `offline` |
| `dnd` | Do not disturb (meeting/training) | `idle`, `away`, `offline` |
| `ringing` | Ringing (call_id present) | `busy` (answer), `idle` (no answer) |
| `busy` | On a call (call_id present) | `wrapup` |
| `wrapup` | After-call work | `idle`, `away`, `dnd` |
| `custom:<name>` | Custom status | `idle`, `away`, `dnd`, `offline` |

```json
{
  "rwi": "1.0",
  "agent_state_changed": {
    "agent_id": "agent-001",
    "from_status": "idle",
    "to_status": "busy",
    "call_id": "call-abc",
    "agent_name": "Alice",
    "agent_extension": "8001",
    "caller": "8001",
    "team_id": "sales",
    "duration_secs": 300,
    "reason_code": "CALL"
  }
}
```

---

### 6.8 DN (Directory Number) Events

#### dn_state_changed

Dispatch: broadcast

Granular extension-level signaling events.

| Field | Type | Description |
|-------|------|-------------|
| `caller` | String | Extension / caller |
| `event_name` | String | Event name (see table below) |
| `system_time` | String | System timestamp |
| `call_id` | Option\<String\> | Associated call ID |
| `agent_id` | Option\<String\> | Agent ID |
| `caller_name` | Option\<String\> | Calling party name/number |
| `callee_name` | Option\<String\> | Called party name/number |
| `reason_code` | Option\<String\> | Reason code |
| `agent_work_mode` | Option\<String\> | Agent work mode |
| `releasing_party` | Option\<String\> | Releasing party (`"1 Local"` / `"2 Remote"`) |
| `vq_name` | Option\<String\> | Virtual queue name |
| `routing_target` | Option\<String\> | Routing target |
| `skill_group` | Option\<String\> | Skill group |
| `extra` | Option\<Map\<String, Value\>\> | Extension fields (omitted when absent) |

**event_name values**:

| event_name | Description | Trigger |
|------------|-------------|---------|
| `REGISTERED` | Extension registered | SIP REGISTER success |
| `DIALING` | Outbound dialing | Agent outbound or manual dial |
| `RINGING` | Ringing | Agent-side ringing |
| `ESTABLISHED` | Call established | Call answered |
| `RELEASED` | Released | Hangup or transfer completed |
| `ABANDONED` | Abandoned | Caller abandoned during ringing |
| `HELD` | Held | Agent held, user hears music |
| `RETRIEVED` | Retrieved from hold | Held party retrieved |
| `PARTYCHANGED` | Multi-party state changed | Conference state changed |
| `PARTYADDED` | Multi-party added | Party added to conference |
| `PARTYDELETED` | Multi-party removed | Party removed from conference |
| `AGENTLOGIN` | Agent login | Agent went from offline to online (CC addon) |
| `AGENTLOGOUT` | Agent logout | Agent went from online to offline (CC addon) |
| `AGENTREADY` | Agent ready | Agent entered idle state (CC addon) |
| `AGENTNOTREADY` | Agent not ready | Agent entered busy/ringing/wrapup etc. (CC addon) |
| `ONHOOK` | On hook | Phone on hook |

> **Note**: Use `event_name` for event routing and matching.

```json
{
  "rwi": "1.0",
  "dn_state_changed": {
    "caller": "80001",
    "event_name": "ESTABLISHED",
    "system_time": "2026-05-14T17:54:49.003Z",
    "call_id": "call-abc",
    "agent_id": "10001",
    "caller_name": "19534519769",
    "callee_name": "39989",
    "extra": {
      "source": "KS",
      "kz_conn_id": "kc-12345",
      "user_data": { "kz_target": "39299", "kz_flowname": "CTC400Customer" }
    }
  }
}
```

#### dn_registered / dn_unregistered

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `caller` | String | Extension number |
| `agent_id` | Option\<String\> | Agent ID |
| `register_time` / `unregister_time` | String | Registration/unregistration timestamp |

---

### 6.9 Call Metadata Events

#### call_metadata_updated

Triggered when call metadata is updated after initial `call_incoming`.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `metadata` | CallMetadata | Metadata (see below) |

**CallMetadata fields**:

| Field | Type | Description |
|-------|------|-------------|
| `root_call_id` | Option\<String\> | Root call ID |
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number |
| `called_phone` | Option\<String\> | Actual called number |
| `dial_direction` | Option\<String\> | Call direction |
| `uuid` | Option\<String\> | Global UUID |
| `routing_path` | Option\<Vec\<String\>\> | Routing path |
| `app_id` | Option\<String\> | IVR application ID |
| `routing_target` | Option\<String\> | Routing target |
| `switch_name` | Option\<String\> | Switch name |

```json
{
  "rwi": "1.0",
  "call_metadata_updated": {
    "call_id": "call-abc",
    "metadata": {
      "root_call_id": "call-root-42",
      "caller_name": "330909",
      "callee_name": "9242000001",
      "called_phone": "018659727661",
      "dial_direction": "inbound",
      "uuid": "uuid-abc-123",
      "routing_path": ["menu:root", "queue:level1"],
      "app_id": "ivr-support",
      "routing_target": "queue:support",
      "switch_name": "SIP_Switch_KS"
    }
  }
}
```

---

### 6.10 Conference Events

#### conference_created / conference_destroyed

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference room ID |

#### conference_member_joined / conference_member_left / conference_member_muted / conference_member_unmuted

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `call_id` | String | Member call ID |
| *+ctx* | | Flat context fields |

#### conference_ended_by_host

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `host_call_id` | String | Host call ID |
| `removed_call_ids` | Vec\<String\> | Removed member call IDs |
| *+ctx* | | Flat context fields |

#### conference_auto_ended

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `reason` | String | End reason |
| *+ctx* | | Flat context fields |

#### conference_error

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `error` | String | Error message |

#### conference_consult_dialing / conference_consult_connected

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Consultation call ID |
| `target` | String | Consultation target |
| *+ctx* | | Flat context fields |

#### conference_merge_requested / conference_merged / conference_merge_failed

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call ID (`merge_requested` includes `consultation_call_id`) |
| `conf_id` | String | Conference ID (`merged`/`merge_failed`) |
| `consultation_call_id` | String | `merge_requested` only: consultation call ID |
| `reason` | String | `merge_failed` only: failure reason |
| *+ctx* | | Flat context fields |

#### conference_seat_replace_started / ...succeeded / ...failed / ...rollback_failed

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `old_call_id` | String | Old member call ID |
| `new_call_id` | String | New member call ID |
| `reason` | String | `failed`/`rollback_failed` only: failure reason |

**Seat replacement event sequence (success path)**:
1. `conference_seat_replace_started`
2. `conference_member_left` (old member leaves)
3. `conference_member_joined` (new member joins)
4. `conference_seat_replace_succeeded`

---

### 6.11 Supervisor Events

#### supervisor_listen_started / supervisor_whisper_started / supervisor_barge_started / supervisor_takeover_started

| Field | Type | Description |
|-------|------|-------------|
| `supervisor_call_id` | String | Supervisor call ID |
| `target_call_id` | String | Target call ID |

#### supervisor_mode_stopped

| Field | Type | Description |
|-------|------|-------------|
| `supervisor_call_id` | String | Supervisor call ID |
| `target_call_id` | String | Target call ID |

---

### 6.12 Parallel Originate Events

#### parallel_originate_started

| Field | Type | Description |
|-------|------|-------------|
| `operation_id` | String | Operation ID |
| `leg_count` | u32 | Number of parallel legs |

#### parallel_originate_leg_ringing / parallel_originate_winner / parallel_originate_leg_cancelled

| Field | Type | Description |
|-------|------|-------------|
| `operation_id` | String | Operation ID |
| `call_id` | String | Leg call ID |
| `destination` | String | Dialed destination |
| `reason` | String | `leg_cancelled` only: cancellation reason |
| *+ctx* | | Flat context fields |

#### parallel_originate_completed

| Field | Type | Description |
|-------|------|-------------|
| `operation_id` | String | Operation ID |
| `winning_call_id` | String | Winning call ID |

#### parallel_originate_failed

| Field | Type | Description |
|-------|------|-------------|
| `operation_id` | String | Operation ID |
| `reason` | String | Failure reason |

---

### 6.13 SIP Signaling Events

#### sip_message_received / sip_notify_received

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `content_type` | String | Content type |
| `body` | String | Message body |
| `event` | String | `sip_notify_received` only: SIP Event header |
| *+ctx* | | Flat context fields |

---

### 6.14 Session System Events

#### call_ownership_changed

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `session_id` | String | Taking-over session ID |
| `mode` | String | Mode (`control`/`listen`/`whisper`/`barge`) |
| *+ctx* | | Flat context fields |

#### session_resumed

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | String | Resumed session ID |
| `last_sequence` | u64 | Client-reported last sequence number |

---

## 7. Event Quick Reference

| Event Type | Dispatch | call_id | Context |
|------------|----------|---------|---------|
| `call_incoming` | fan_out | yes | own fields |
| `call_ringing` | owner | yes | +ctx |
| `call_early_media` | owner | yes | +ctx |
| `call_answered` | owner | yes | +ctx |
| `call_bridged` | owner | leg_a | — |
| `call_unbridged` | owner | yes | +ctx |
| `call_transferred` | owner | yes | +ctx |
| `call_transfer_accepted` | owner | yes | +ctx |
| `call_transfer_failed` | owner | yes | +ctx |
| `call_hangup` | owner | yes | +ctx |
| `call_no_answer` | owner | yes | +ctx |
| `call_busy` | owner | yes | +ctx |
| `media_hold_started` | owner | yes | +ctx |
| `media_hold_stopped` | owner | yes | +ctx |
| `media_ringback_passthrough_started` | owner | yes | — |
| `media_ringback_passthrough_stopped` | owner | yes | — |
| `media_play_started` | owner | yes | +ctx |
| `media_play_finished` | owner | yes | +ctx |
| `media_stream_started` | owner | yes | +ctx |
| `media_stream_stopped` | owner | yes | +ctx |
| `record_started` | owner | yes | +ctx |
| `record_paused` | owner | yes | +ctx |
| `record_resumed` | owner | yes | +ctx |
| `record_stopped` | owner | yes | own fields + enrich |
| `record_failed` | owner | yes | +ctx |
| `recording_metadata_available` | owner | yes | — |
| `dtmf` | fan_out | yes | +ctx |
| `dtmf_collected` | owner | yes | +ctx |
| `dtmf_collection_timeout` | owner | yes | +ctx |
| `ivr_node_entered` | fan_out | yes | +ctx |
| `ivr_node_exited` | fan_out | yes | +ctx |
| `ivr_flow_transitioned` | fan_out | yes | +ctx |
| `ivr_flow_completed` | fan_out | yes | +ctx |
| `ivr_step_trace` | fan_out | yes | — |
| `queue_joined` | owner/broadcast | yes | +ctx |
| `queue_position_changed` | owner | yes | +ctx |
| `queue_agent_offered` | broadcast | yes | +ctx |
| `queue_agent_connected` | owner | yes | +ctx |
| `queue_left` | broadcast | yes | +ctx |
| `queue_wait_timeout` | owner | yes | +ctx |
| `queue_overflowed` | owner | yes | +ctx |
| `queue_voicemail_redirected` | owner | yes | +ctx |
| `queue_candidates_found` | owner | yes | +ctx |
| `queue_agent_ringing` | owner | yes | +ctx |
| `queue_agent_no_answer` | owner | yes | +ctx |
| `queue_agent_rejected` | owner | yes | +ctx |
| `queue_fallback_executed` | owner | yes | +ctx |
| `queue_alert` | broadcast | — | — |
| `agent_state_changed` | broadcast | optional | — |
| `dn_state_changed` | broadcast | optional | — |
| `dn_registered` | broadcast | — | — |
| `dn_unregistered` | broadcast | — | — |
| `call_metadata_updated` | owner | yes | — |
| `conference_created` | broadcast | — | — |
| `conference_member_joined` | broadcast | yes | +ctx |
| `conference_member_left` | broadcast | yes | +ctx |
| `conference_member_muted` | broadcast | yes | +ctx |
| `conference_member_unmuted` | broadcast | yes | +ctx |
| `conference_destroyed` | broadcast | — | — |
| `conference_ended_by_host` | broadcast | — | +ctx |
| `conference_auto_ended` | broadcast | — | +ctx |
| `conference_error` | broadcast | — | — |
| `conference_consult_dialing` | owner | yes | +ctx |
| `conference_consult_connected` | owner | yes | +ctx |
| `conference_merge_requested` | fan_out | yes | +ctx |
| `conference_merged` | fan_out | yes | +ctx |
| `conference_merge_failed` | fan_out | yes | +ctx |
| `conference_seat_replace_started` | fan_out | yes | — |
| `conference_seat_replace_succeeded` | fan_out | yes | — |
| `conference_seat_replace_failed` | fan_out | yes | — |
| `conference_seat_replace_rollback_failed` | fan_out | yes | — |
| `supervisor_listen_started` | owner | — | — |
| `supervisor_whisper_started` | owner | — | — |
| `supervisor_barge_started` | owner | — | — |
| `supervisor_takeover_started` | owner | — | — |
| `supervisor_mode_stopped` | owner | — | — |
| `parallel_originate_started` | owner | — | — |
| `parallel_originate_leg_ringing` | owner | yes | +ctx |
| `parallel_originate_winner` | owner | yes | +ctx |
| `parallel_originate_leg_cancelled` | owner | yes | +ctx |
| `parallel_originate_completed` | owner | yes | — |
| `parallel_originate_failed` | owner | — | — |
| `sip_message_received` | owner | yes | +ctx |
| `sip_notify_received` | owner | yes | +ctx |
| `call_ownership_changed` | owner | yes | +ctx |
| `session_resumed` | owner | — | — |

---

## 8. Developer Examples

### Python Webhook Receiver

```python
from http.server import HTTPServer, BaseHTTPRequestHandler
import json

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length))

        event_type = body["event_type"]
        call_id = body["call_id"]

        print(f"[{event_type}] call_id={call_id}")

        if event_type == "recording_metadata_available":
            meta = body["event"]["recording_metadata_available"]["metadata"]
            print(f"  download: {meta['download_url']}")
            print(f"  file_size: {meta['file_size']}")

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')

HTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
```

### Python WebSocket Real-time Listener

```python
import asyncio, json
from websockets import connect

async def main():
    async with connect(
        "ws://pbx.example.com/rwi/v1",
        additional_headers={"Authorization": "Bearer your-token"},
        subprotocols=["rwi-v1"],
    ) as ws:
        await ws.send(json.dumps({
            "rwi": "1.0",
            "action_id": "sub-001",
            "action": "session.subscribe",
            "params": {"contexts": ["*"]}
        }))

        async for msg in ws:
            payload = json.loads(msg)
            for key, data in payload.items():
                if key == "rwi":
                    continue
                print(f"[{key}] {json.dumps(data, ensure_ascii=False)}")

asyncio.run(main())
```

### JavaScript / Node.js

```javascript
const ws = new WebSocket("ws://pbx.example.com/rwi/v1", "rwi-v1", {
  headers: { Authorization: "Bearer your-token" }
});

ws.onopen = () => {
  ws.send(JSON.stringify({
    rwi: "1.0",
    action_id: "sub-001",
    action: "session.subscribe",
    params: { contexts: ["*"] }
  }));
};

ws.onmessage = (event) => {
  const payload = JSON.parse(event.data);
  for (const [eventType, eventData] of Object.entries(payload)) {
    if (eventType === "rwi") continue;
    console.log(`[${eventType}] call=${eventData.call_id}`, eventData);
  }
};
```

---

## 9. Auxiliary Structures

These structs are used as nested references and are not emitted as standalone events.

### IvrNodeInfo

| Field | Type | Description |
|-------|------|-------------|
| `node_id` | String | Node ID |
| `node_name` | String | Node name |
| `node_type` | String | Node type |
| `routing_target` | Option\<String\> | Routing target |
| `previous_node_id` | Option\<String\> | Previous node ID |
| `next_node_id` | Option\<String\> | Next node ID |
| `duration_ms` | Option\<u32\> | Dwell time |
| `result_value` | Option\<String\> | DTMF/result |

### IvrFlowContext

| Field | Type | Description |
|-------|------|-------------|
| `app_id` | String | IVR application ID |
| `routing_path` | Vec\<String\> | Routing path |
| `service_type` | Option\<String\> | Service type |
| `customer_type` | Option\<String\> | Customer type |

---

**Document version**: v1.0  
**Last updated**: 2026-06-05  
**Source code**: `src/rwi/proto.rs`
