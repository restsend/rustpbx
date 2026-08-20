# RWI Events Developer Reference

> Source code: `src/rwi/event.rs` (event structs) and `src/rwi/proto.rs` (EventCallContext / RecordingMetadata) | Protocol version: `1.0`

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
# empty = all events (recommended). To allow-list, use valid event types.
# Note: agent status is "agent_state_changed" (the old "dn_state_changed" was
# removed); recording data (download URL, file size) is delivered via
# "recording_metadata_available" and "record_end" — "record_stopped" alone
# carries no recording URL.
# Example allow-list:
# events = ["call_hangup", "record_stopped", "recording_metadata_available", "record_end", "agent_state_changed"]
events = []
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

Event fields are flattened directly as top-level JSON keys, with an
embedded `event_type` key identifying the event:

```json
{
  "event_type": "call_ringing",
  "call_id": "call-abc123",
  "caller_name": "330909",
  "callee_name": "9242000001",
  "direction": "inbound"
}
```

> There is no `"rwi"` or event-name wrapper object; clients dispatch on the
> `event_type` field.

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
| `root` | Option\<Object\> | Root call identity (see below) |

**Root call (`root`)** — nested object identifying the root call of this call
tree:

| Field | Type | Description |
|-------|------|-------------|
| `caller` | Option\<String\> | Root call caller SIP URI |
| `caller_name` | Option\<String\> | Root call caller name |
| `callee` | Option\<String\> | Root call callee SIP URI |
| `callee_name` | Option\<String\> | Root call callee name |
| `call_id` | Option\<String\> | Root call identifier |
| `start_time` | Option\<String\> | Root call start time (RFC3339) |

Populated with the session's own call context (`root = self`). Transferred
legs that run in a separate session keep their own context — there is no
cross-session root propagation.

**Notes**: the flat context never contains `agent_id`/`agent_name` — those are
event-specific fields (e.g. `cc_*` events, `record_stopped`) that only appear
when the event itself carries them. A `call_*` event without agent involvement
never has agent-related values.

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
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number / DNIS |
| `called_phone` | Option\<String\> | Actual called number (outbound scenario) |
| `app_id` | Option\<String\> | IVR application ID |
| `routing_target` | Option\<String\> | Routing target |
| `uuid` | Option\<String\> | Global UUID (for recording linkage) |
| `routing_path` | Option\<Vec\<String\>\> | Routing path sequence |
| `session_id` | Option\<String\> | Enrichment: logical-call root session id |

> **Note**: `call_incoming` uses `dial_direction`; other events' context uses `direction`.
> Former field `root_call_id` was removed; use enrichment `session_id` for multi-leg correlation.

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
    "session_id": "call-abc",
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

#### call_initiated

Delivery: call_owner

First event sent to the session owner when an outbound call is initiated
(RWI `call.originate` / outbound dial).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `callee` | String | Destination URI |
| *+ctx* | | Flattened context |

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
| `hangup_by` | Option\<String\> | Normalized initiator: `agent` \| `caller` \| `system` \| `transfer` \| `unknown`. Same vocabulary as `cc_hangup.hangup_by`. A callee hangup is reported as `agent` only when the call actually involved a CC agent (queue-routed or `resolved_agent_id`); otherwise it is `callee`. |
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
    "hangup_by": "caller",
    "sip_status": null,
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "direction": "inbound"
  }
}
```

#### cc_hangup

Contact-center layer hangup (CC-routed calls only). Emitted alongside
`call_hangup`; named `cc_hangup` for consistency with the core event. `reason`
uses the **same Display vocabulary** as `call_hangup.reason` (e.g. `caller`,
`callee`, `abandoned` — NOT the Debug form). `hangup_by` makes it explicit
whether the agent, the caller, the system, or a transfer ended the call —
this is critical for contact-center reporting.

Dispatch: broadcast (delivered to the configured `[rwi_webhook]`). Broadcast
events carry the primary call's flat context (`caller`/`callee`/names/
`direction`) via gateway enrichment, like all call-scoped events.

cc_* events (including `cc_ringing`/`cc_answered`) are emitted **only when the
call actually involves a registered CC agent** — a plain extension-to-extension
call produces no `cc_*` events.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `agent_id` | Option\<String\> | CC agent identifier (callee leg) |
| `agent_name` | Option\<String\> | CC agent display name |
| `queue_id` | Option\<String\> | Queue/skill-group the call was routed through |
| `reason` | String | Normalized reason, same vocabulary as `call_hangup.reason` |
| `hangup_by` | Option\<String\> | `agent` \| `caller` \| `system` \| `transfer` \| `unknown` |
| `duration_secs` | u64 | Talk time in seconds (0 for unanswered) |
| *+ctx* | | Flat context fields |

`cc_ringing` / `cc_answered` / `cc_held` / `cc_unheld` also carry `agent_id`
(canonical agent id, resolved from endpoint → primary_endpoint → agent_id) and
`agent_name`.

```json
{
  "rwi": "1.0",
  "event_type": "cc_hangup",
  "event": {
    "call_id": "call-abc",
    "agent_id": "1001",
    "queue_id": "support",
    "reason": "callee",
    "hangup_by": "agent",
    "duration_secs": 42
  }
}
```

> Previously this event was named `cc_ended` and carried `reason` as the Debug
> form of the internal enum (e.g. `"ByCallee"`). Both were normalized to match
> `call_hangup`.

### 6.2 Transfer Events

#### call_transferred / call_transfer_accepted

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `transfer_target` | Option\<String\> | Original transfer target string (e.g. `queue:queue-name?target=skillgroup:tech-support_G`). `None` when the target is unavailable (e.g. SIP REFER Replaces takeover). |
| *+ctx* | | Flat context fields |

#### call_transfer_failed

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `sip_status` | Option\<u16\> | SIP status code |
| `reason` | Option\<String\> | Failure reason |
| `transfer_target` | Option\<String\> | Original transfer target string (see above) |
| *+ctx* | | Flat context fields |

### 6.3 Media Events

#### media_hold_started / media_hold_stopped

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| *+ctx* | | Flat context fields |

> `media_stream_started` / `media_stream_stopped` were removed together with
> the `media.stream_start` / `media.inject_start` commands. Real-time
> bidirectional PCM now uses `call.transfer` → a `voip_bridge:` WebSocket
> endpoint (inbound and outbound calls); these events no longer exist.

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
| `extra` | Option\<Object\> | Extra data (extension field, defaults to `null`) |
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

> Note: `record_stopped` does not carry full typed flat context; CallMetaStore
> enrichment still injects `session_id` (and other missing keys). Former field
> `root_call_id` was removed.

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
    "session_id": "call-root-42"
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

**RecordingMetadata fields** (typed fields + `extra` pass-through bag):

| Field | Type | Description |
|-------|------|-------------|
| `filename` | String | Recording filename |
| `file_size` | u64 | File size in bytes |
| `download_url` | Option\<String\> | Download URL |
| `caller_name` / `callee_name` | Option\<String\> | Caller / callee numbers |
| `call_type` | String | Call type |
| `call_start_time` / `call_end_time` / `upload_time` | Option\<String\> | Call start / end / upload time |
| *(any other key)* | String | `extra` pass-through bag (`#[serde(flatten)]`): flat string keys written by addons (`agent_id`, `queue_id`, `tenant_id`, `switch_flag`, ...) are forwarded verbatim; the core does not name them |

> Note: there is no typed `unique_id` field; business fields like `agent_id`
> depend on the addon writing them into `extra`.

> `agent_id` / `agent_name` are populated from the session extensions when the
> call was routed to a CC agent (`agent_id` is the canonical agent id resolved
> via endpoint → primary_endpoint → agent_id; `agent_name` is the agent display
> name). For calls without CC agent involvement they are absent.

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
      "session_id": "call-root-42"
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

> **Also emitted on session termination**: when the sip_session is terminated mid-flow (caller hangup, system cancel, etc.), the built-in (tree-mode) IVR emits this event to record the node the caller was on. In that case `hangup_reason` is populated (e.g. `cancelled`, `remote_hangup`, `hangup`, `error`) and `call_result` is `"hangup"`.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `node_id` | String | Node ID |
| `node_name` | String | Node name |
| `result_value` | Option\<String\> | User DTMF or branch result |
| `duration_ms` | u32 | Node dwell time in milliseconds |
| `exit_time` | String | Exit timestamp |
| `next_node_id` | Option\<String\> | Next node ID |
| `hangup_reason` | Option\<String\> | Hangup reason (on session termination: `cancelled`/`remote_hangup`/`hangup`, etc.) |
| `call_result` | Option\<String\> | Call result |
| *+ctx* | | Flat context fields |


#### ivr_flow_completed

Dispatch: fan_out_to_context

IVR flow completes (terminal action executed: Transfer, Queue, Voicemail, Hangup).

> **Also emitted on session termination**: when the built-in (tree-mode) IVR is terminated mid-flow by the sip_session (caller hangup `remote_hangup`, system cancel `cancelled`, etc.), it is emitted with `final_result` set to the termination reason and `total_nodes_traversed` populated. `final_result` values: `transferred`, `queue`, `voicemail`, `hangup`, `abandoned`, `cancelled`, `remote_hangup`, `error`, etc.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `app_id` | String | IVR application ID |
| `total_nodes_traversed` | u32 | Total nodes traversed |
| `total_duration_ms` | u32 | Total IVR duration in milliseconds |
| `final_result` | String | Final result (`transferred`, `voicemail`, `abandoned`, `cancelled`, `remote_hangup`, etc.) |
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

> **Session-end entry (`session_end`)**: when the IVR session ends (including caller hangup `RemoteHangup` and system cancel `Cancelled`), an extra trace entry with `trigger.type="session_end"` is emitted. `action_type`/`step_id`/`step_name` record the last executed node, and `end_reason`/`end_detail` describe how the whole session ended. The external provider `/end` webhook is **not** called on `RemoteHangup`/`Cancelled` (the local trace event is still emitted).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `session_id` | String | Session ID |
| `caller` | String | Caller |
| `callee` | String | Callee |
| `step_index` | u32 | Step index |
| `trigger` | Object | Structured trigger info for this step, see below |
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
| `end_reason` | Option\<String\> | Present only on the session-end (`session_end`) entry; identifies how the whole IVR session ended (`normal`, `transfer`, `transfer_to_queue`, `hangup`, `user_hangup`, `timeout`, `error`, etc.) |
| `end_detail` | Option\<String\> | Companion detail for `end_reason` (e.g. transfer target, error message) |

> **`trigger` field**:
>
> Describes what caused the current step to execute, as an object:
>
> ```json
> { "type": "dtmf", "detail": { "digit": "2" } }
> ```
>
> | Sub-field | Type | Description |
> |-----------|------|-------------|
> | `type` | String | Trigger source type: `session_start`, `session_end`, `dtmf`, `dtmf_menu`, `dtmf_menu_timeout`, `audio_complete`, `action_execute`, `chained`, `api_response`, `phone_collected`, `recording_complete`, `input_voice`, `error`, `dtmf_menu_invalid`, `unknown` |
> | `detail` | Option\<JSON Object\> | Structured trigger detail, omitted when none. Common values: DTMF → `{"digit":"2"}`; API response → `{"status":200}`; phone collection → `{"number":"13800138000"}` |
>
> **Timing fields**:
> - `step_start_time` — when the current step started (previous step end or session start)
> - `step_end_time` — when the step ended (only on completion)
>
> **Duration fields**:
> - `duration_ms` — step execution duration (ms), always present, includes provider round-trip and action execution time

---

### 6.6 Queue / ACD Events

> **Event origin**: Queue-related events come in two families, produced by different subsystems and may co-occur:
> - **`queue_*` (queue lifecycle)**: produced by the Queue app (`src/call/app/queue.rs`) **and** the CC ACD engine bridge. Covers the generic lifecycle: join, ringing, connected, abandon, timeout, fallback.
> - **`skill_group_*` (skill-group scheduling decisions)**: produced **exclusively** by the CC addon's ACD adapter (`src/addons/cc/agent_registry_adapter.rs`) when the queue asks the ACD for an agent. Fires only when the CC addon is active and skill routing is used. The ACD-engine `queue_*` bridge intentionally does **not** emit `skill_group_*` (single source, no duplicates).
>
> Typical event sequence for a skill-group-routed call:
> `queue_joined` → `skill_group_candidates_found` → `skill_group_call_queued` (only when no agent is immediately available) → `skill_group_agent_assigned` → `queue_agent_offered` → `queue_agent_connected`
>
> `skill_group_call_abandoned` fires when the caller hangs up while still queued; `skill_group_service_unavailable` fires on queue timeout or fallback. Both are reported by the Queue app through the `AgentRegistry` lifecycle hooks (`notify_call_abandoned` / `notify_call_timeout` / `notify_call_fallback`), which the CC adapter maps to the RWI events.

All queue events carry flat context fields.

> **session_id correlation (since 2026-08)**: all call-scoped events
> (`queue_*` / `skill_group_*` / `cc_*` / `call_*`) are enriched from
> CallMetaStore with a top-level `session_id` — the root logical-call id
> (first INVITE Call-ID; stable across transfer / dispatch / consult).
> `call_id` is the current leg and changes for transfer children.
> Former typed field `root_call_id` was removed. See
> [session_id_correlation.md](./session_id_correlation.md).

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

#### skill_group_candidates_found

Dispatch: broadcast

Emitted when the ACD scheduler finds candidate agents for a skill group.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | Option\<String\> | Skill group ID (`Some` for the explicit `skill-group:{id}` path; `None` for autonomous skill routing) |
| `candidates` | Vec\<String\> | Candidate agent ID list |
| `trace_id` | String | Trace ID |
| *+ctx* | | Flat context fields |

#### skill_group_agent_assigned

Dispatch: broadcast

Emitted when the ACD scheduler decides to assign an agent to the call. This fires
for an ACD `Assign` decision **and** for the strategy-picked first agent when no
inline ACD policy is configured ("first agent selected by the strategy").

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | Option\<String\> | Skill group ID |
| `agent_id` | String | Assigned agent ID |
| `dispatch_reason` | String | `regular` / `forced_available` / `overflow` |
| `trace_id` | String | Trace ID |
| *+ctx* | | Flat context fields |

#### skill_group_no_agent

Dispatch: broadcast

Emitted when the ACD scheduler cannot provide an agent for the skill group.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | Option\<String\> | Skill group ID |
| `reason` | String | Reason (`no_candidates` no matching agent / `acd_blocked` blocked by ACD policy / `no_strategy_match` strategy picked none) |
| *+ctx* | | Flat context fields |

#### skill_group_call_queued

Dispatch: broadcast

Emitted when the call enters the skill-group queue because no agent was
immediately available. Fires on an ACD `Wait` decision (with real `position`/
`ewt_secs`) **or**, when no ACD policy is configured, whenever routing finds no
currently available agent (best-effort `position`/`ewt_secs`).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | String | Skill group ID |
| `position` | usize | Queue position |
| `ewt_secs` | u32 | Estimated wait time (seconds) |
| `reason` | String | `no_agent_available` |
| `trace_id` | String | Trace ID |
| *+ctx* | | Flat context fields |

#### skill_group_call_abandoned

Dispatch: broadcast

Emitted when the caller hangs up while still waiting in the skill-group queue
(before any agent answered).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | String | Skill group ID |
| `waited_secs` | u64 | Time waited before abandoning |
| `position` | usize | Queue position at abandon |
| `trace_id` | String | Trace ID |
| *+ctx* | | Flat context fields |

#### skill_group_service_unavailable

Dispatch: broadcast

Emitted when a queued call could not be serviced (queue timeout or fallback).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | String | Skill group ID |
| `reason` | String | `timeout` / fallback reason |
| `attempts` | u32 | Retry attempts |
| `waited_secs` | u64 | Time waited |
| `fallback_action` | String | Executed fallback action |
| `trace_id` | String | Trace ID |
| *+ctx* | | Flat context fields |

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
| `call_initiated` | owner | ✅ | Outbound call initiated |
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
| `media_play_started` | owner | yes | +ctx |
| `media_play_finished` | owner | yes | +ctx |
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
| `ivr_flow_completed` | fan_out | yes | +ctx |
| `ivr_step_trace` | fan_out | yes | — |
| `queue_joined` | owner/broadcast | yes | +ctx |
| `queue_position_changed` | owner | yes | +ctx |
| `queue_agent_offered` | broadcast | yes | +ctx |
| `queue_agent_connected` | owner | yes | +ctx |
| `queue_left` | broadcast | yes | +ctx |
| `queue_wait_timeout` | owner | yes | +ctx |
| `queue_candidates_found` | owner | yes | +ctx |
| `queue_agent_ringing` | owner | yes | +ctx |
| `queue_agent_no_answer` | owner | yes | +ctx |
| `queue_agent_rejected` | owner | yes | +ctx |
| `queue_fallback_executed` | owner | yes | +ctx |
| `queue_alert` | broadcast | — | — |
| `skill_group_candidates_found` | broadcast | yes | +ctx |
| `skill_group_agent_assigned` | broadcast | yes | +ctx |
| `skill_group_no_agent` | broadcast | yes | +ctx |
| `skill_group_call_queued` | broadcast | yes | +ctx |
| `skill_group_call_abandoned` | broadcast | yes | +ctx |
| `skill_group_service_unavailable` | broadcast | yes | +ctx |
| `agent_state_changed` | broadcast | optional | +ctx |
| `cc_ringing` | broadcast | yes | +ctx |
| `cc_answered` | broadcast | yes | +ctx |
| `cc_hangup` | broadcast | yes | +ctx |
| `cc_held` | broadcast | yes | +ctx |
| `cc_unheld` | broadcast | yes | +ctx |
| `conference_created` | broadcast | — | — |
| `conference_member_joined` | broadcast | yes | +ctx |
| `conference_member_left` | broadcast | yes | +ctx |
| `conference_member_muted` | broadcast | yes | +ctx |
| `conference_member_unmuted` | broadcast | yes | +ctx |
| `conference_destroyed` | broadcast | — | — |
| `conference_ended_by_host` | broadcast | — | +ctx |
| `conference_error` | broadcast | — | — |
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
| `sip_message_received` | owner | yes | +ctx |
| `sip_notify_received` | owner | yes | +ctx |

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
            meta = body["event"]  # same flat payload as the WS event
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
**Last updated**: 2026-06-23  
**Source code**: `src/rwi/event.rs`
