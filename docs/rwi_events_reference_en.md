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
# Retries after a failed push (transport error, 5xx or 429). Other 4xx are
# permanent and return immediately. Backoff doubles from 200 ms. Hard cap 5.
retries = 2
# Opt-in: track event queueing latency (enqueued -> handler dequeued) in the
# rwi_event_queue_latency_seconds histogram. Disabled by default.
track_queue_latency = true
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
| `timeout_ms` | u64 | 5000 | HTTP request timeout in milliseconds (per attempt) |
| `headers` | HashMap | (optional) | Custom HTTP headers sent with every request |
| `events` | Vec\<String\> | [] (all) | Event type whitelist; empty forwards all events |
| `retries` | u32 | 0 | Retries after a failed push (transport error, 5xx, 429); hard cap 5; exponential backoff from 200 ms |
| `track_queue_latency` | bool | false | Record the queueing-wait histogram `rwi_event_queue_latency_seconds` |

The webhook handler runs on a dedicated tokio runtime so its HTTP push never
contends with the SIP runtime. The worker count and the event queue length
are configured under `[proxy]`:

| Key | Default | Description |
|-----|---------|-------------|
| `[proxy] rwi_webhook_worker_threads` | 2 | Dedicated tokio workers for the webhook push consumer |
| `[proxy] rwi_webhook_channel_size` | 512 | Event queue length (broadcast channel capacity) |

### Webhook Metrics

| Metric | Type | Labels | Description |
|-------|------|--------|-------------|
| `rwi_event_enqueued_total` | Counter | `event_type` | Events pushed into the queue by gateway dispatch |
| `rwi_events_pushed_total` | Counter | `event_type` | Events delivered with a 2xx response |
| `rwi_events_push_failed_total` | Counter | `event_type` | Pushes that errored or returned non-2xx |
| `rwi_events_push_retries_total` | Counter | `event_type` | Retry attempts after a failed push |
| `rwi_events_dropped_total` | Counter | - | Events lost to queue lag (consumer fell behind) |
| `rwi_event_queue_size` | Gauge | - | Configured queue capacity |
| `rwi_event_queue_current` | Gauge | - | Events currently queued (sampled every 5 s) |
| `rwi_event_queue_latency_seconds` | Histogram | `event_type` | Queueing wait (enqueued -> handler dequeued); opt-in via `track_queue_latency` |

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
| `agent_id` | Option\<String\> | CC agent identifier — present **only** when the call actually involves a registered CC agent |
| `agent_name` | Option\<String\> | CC agent display name (same condition as `agent_id`) |
| `queue_id` | Option\<String\> | Queue the call is being served by, when any |
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

**Notes**: the flat context carries `agent_id`/`agent_name` **only when the
call actually involves a registered CC agent** — the CC session hook resolves
and publishes the attribution (canonical agent id, resolved from endpoint →
primary_endpoint → agent_id) and the proxy session syncs it into the call
meta. Calls without CC agent involvement never carry agent-related values.
This is how the former separate `cc_ringing` / `cc_answered` / `cc_hangup` /
`cc_held` / `cc_unheld` events are now expressed: as the unified `call_*`
lifecycle events enriched with agent context.

**Notes**:
- `ani` vs `caller`: `ani` is a plain number (for business logic), `caller` is the full SIP URI
- `dnis` vs `callee`: same distinction
- Context is injected by `CallMetaStore` at gateway dispatch time — event producers never fill it manually

### user_data (session user data injection)

On top of the fields above, the gateway injects the **session user data**
object into **every call-scoped event** under the `user_data` key (a nested
object, not flattened):

| Field | Type | Description |
|-------|------|-------------|
| `user_data` | Option\<Object\> | Session user data — replaced wholesale via REST `PUT /calls/active/{session_id}/userdata` or the RWI `call.set_userdata` command; omitted when unset |

Business systems can write CRM ticket ids, customer profiles, etc. at any
point of the call; every subsequent event (including webhooks) then carries
the object automatically, and it is persisted into the CDR
`metadata["user_data"]` when the call ends. Replacements are announced via
the `call_userdata_updated` event (carrying the full new value).

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
  "params": {}
}
```

Server buffers the latest 1000 events (60-second retention). After
reconnection, all cached events are replayed (`call.resume` with a `call_id`
replays just that call's events). Clients dedupe on `call_id` + `event_type`
as needed.

### Webhook Deduplication

The webhook handler deduplicates using `(call_id, timestamp)` tuples in a 4096-entry ring buffer. Duplicate events are silently dropped.

---

## 6. Complete Event Dictionary

> In the tables below, `+ctx` means the event carries flat context fields.
> `?` indicates an `Option<T>` field — omitted from JSON when `null`.

### 6.1 Call Lifecycle

#### call_created

Dispatch: call_owner

A call was created and entered the dialing (calling) phase — emitted for
inbound INVITEs and API originates (`call.originate` / outbound dial) alike.
First event in any call flow.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Unique call identifier |
| `context` | String | Dialplan context |
| `caller` | String | Caller SIP URI |
| `callee` | String | Callee SIP URI |
| `trunk` | Option\<String\> | SIP trunk name |
| `sip_headers` | Map\<String, String\> | Whitelisted SIP headers |
| `caller_name` | Option\<String\> | Calling party number |
| `callee_name` | Option\<String\> | Dialed number / DNIS |
| `called_phone` | Option\<String\> | Actual called number (outbound scenario) |
| `app_id` | Option\<String\> | IVR application ID |
| `routing_target` | Option\<String\> | Routing target |
| `uuid` | Option\<String\> | Global UUID (for recording linkage) |
| `routing_path` | Option\<Vec\<String\>\> | Routing path |
| `session_id` | Option\<String\> | Enrichment: logical-call root session id |
| `direction` | Option\<String\> | Enrichment: `inbound` / `outbound` / `internal` |

> **Note**: all call-scoped events carry the same `direction` context field
> (injected by `CallMetaStore` enrichment). Use enrichment `session_id` for
> multi-leg correlation.

```json
{
  "rwi": "1.0",
  "call_created": {
    "call_id": "call-abc",
    "context": "inbound",
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "direction": "inbound",
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

#### call_ringing / call_answered / call_unbridged / call_no_answer / call_busy

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `early_media` | bool | `call_ringing` only: `true` = provisional response carried SDP (183 Session Progress or 180 with SDP, i.e. early media); `false` = plain 180 Ringing. The former separate `call_early_media` event was merged into this field |
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
    "direction": "inbound",
    "agent_id": "1001",
    "queue_id": "support"
  }
}
```

> When the call involves a registered CC agent, the context carries
> `agent_id`/`agent_name`/`queue_id` — this replaces the former separate
> `cc_ringing` / `cc_answered` events. `call_ringing` is emitted **once per
> provisional response**; consumers tell ringback from early media by the
> `early_media` flag instead of a separate event.

#### call_held / call_unheld

Dispatch: call_owner

A call leg was put on hold / retrieved from hold (explicit Hold command or an
inbound re-INVITE with `sendonly`/`inactive`). These replace the former
`cc_held` / `cc_unheld` events; agent attribution arrives via the flat
context when a CC agent participates.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `leg_id` | String | Held/resumed leg (`caller` / `callee` / ...) |
| *+ctx* | | Flat context fields |

#### call_userdata_updated

Dispatch: call_owner

The session user data object was **replaced wholesale** (REST
`PUT /calls/active/{session_id}/userdata` or RWI `call.set_userdata`).
Carries the complete new object — consumers track changes by replacing their
local copy (no incremental merge semantics). The new value also rides every
subsequent call-scoped event under `user_data` and is persisted into the CDR
`metadata["user_data"]` when the call ends.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call session identifier (session_id) |
| `user_data` | Object | Full new user data (JSON object, ≤ 16 KiB serialized) |
| *+ctx* | | Flat context fields |

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
| `hangup_by` | Option\<String\> | Normalized initiator: `agent` \| `caller` \| `system` \| `transfer` \| `unknown`. A callee hangup is reported as `agent` only when the call actually involved a CC agent (queue-routed or `resolved_agent_id`); otherwise it is `callee`. |
| `sip_status` | Option\<u16\> | SIP response code |
| `duration_secs` | Option\<u64\> | Talk time in seconds (answer → hangup); omitted when the call was never answered |
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
    "duration_secs": 42,
    "caller": "sip:13800138000@pbx.local",
    "callee": "sip:4000@pbx.local",
    "caller_name": "13800138000",
    "callee_name": "4000",
    "direction": "inbound",
    "agent_id": "1001",
    "queue_id": "support"
  }
}
```

> **CC agent calls**: when the call involved a registered CC agent, the hangup
> context carries `agent_id` / `agent_name` / `queue_id` (and `hangup_by`
> reports `agent` for agent-initiated hangups). This replaces the former
> separate `cc_hangup` event.

> Historically there was a separate `cc_hangup` event (before that,
> `cc_ended` carrying `reason` as the Debug form of the internal enum, e.g.
> `"ByCallee"`). Both were first normalized to match `call_hangup`, then
> folded into `call_hangup` entirely.

##### Migration table: former `cc_*` events → unified `call_*`

The CC addon's separate call lifecycle events were removed. Agent context now
arrives via the flat context enrichment (`agent_id` / `agent_name` /
`queue_id`) on the core events, present **only when a registered CC agent
participates**:

| Former event | Unified expression | Notes |
|---|---|---|
| `cc_ringing` | `call_ringing` (+ctx) | `early_media` is the core event's own flag; one event per provisional response |
| `cc_answered` | `call_answered` (+ctx) | Fires at the answer moment per flow: no-app `accept_call`, originate 200 OK, or the queue-agent leg connect (LegConnected) |
| `cc_hangup` | `call_hangup` (+ctx, `duration_secs`) | `reason`/`hangup_by` keep the same vocabulary; `duration_secs` is omitted (not 0) for unanswered calls |
| `cc_held` | `call_held` (+ctx) | `leg_id` unchanged |
| `cc_unheld` | `call_unheld` (+ctx) | `leg_id` unchanged |

Dispatch changed from broadcast to `call_owner`; webhook / event-tap delivery
is unaffected (all dispatch modes forward there). Events now also enter the
resume cache (replayable after reconnect). **Webhook allow-lists**
(`[rwi_webhook].events`) referencing `cc_*` names must be updated to the
`call_*` names — unknown names silently filter everything out.

### 6.2 Transfer Events

#### call_transferred / call_transfer_accepted

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `transfer_target` | Option\<String\> | Original transfer target string (e.g. `queue:queue-name?target=skillgroup:tech-support_G`, or the bare number of a route-table hand-off). `None` when the target is unavailable (e.g. SIP REFER Replaces takeover). |
| `transfer_target_type` | Option\<String\> | Resolved target kind: `queue` \| `ivr` \| `route_point` \| `voicemail` \| `conference` \| `bridge` \| `sip`. Omitted when unknown. |
| `transfer_source` | Option\<Object\> | Flow origin captured at transfer time (`call_transferred` only). Nested object, see below. |
| *+ctx* | | Flat context fields |

`transfer_source` nested fields (all optional):

| Field | Type | Description |
|-------|------|-------------|
| `source_type` | String | `ivr` (transfer out of a running IVR flow) \| `queue` (transfer by a queue-served agent) \| `agent` (transfer attributed to a known agent leg). Only these three values are produced today. |
| `name` | Option\<String\> | Source IVR name (e.g. `main-ivr`) or queue name (e.g. `sales`) |
| `ivr_node_id` | Option\<String\> | IVR node the call was at when transferred |
| `agent_id` | Option\<String\> | Agent that initiated the transfer, when known |

Example — blind transfer from an IVR node into a queue:

```json
{
  "event_type": "call_transferred",
  "call_id": "a1b2c3",
  "transfer_target": "queue:sales",
  "transfer_target_type": "queue",
  "transfer_source": {
    "source_type": "ivr",
    "name": "main-ivr",
    "ivr_node_id": "menu-2"
  }
}
```

Notes:

- Blind transfers to in-session application targets (`queue:` / `ivr:` /
  `toivr:` / `voicemail:` / `conference:`) emit `call_transferred` when the
  hand-off completes; SIP-URI targets emit on dial/REFER success.
- A blind transfer to a **bare number** that the route table maps to a queue
  or application starts that flow in-session (target type `queue` / `ivr`,
  original number kept in `transfer_target`). Gated by
  `proxy.route_originated_calls` for both CTI/API transfers and phone REFERs.

#### call_transfer_failed

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `sip_status` | Option\<u16\> | SIP status code |
| `reason` | Option\<String\> | Failure reason |
| `transfer_target` | Option\<String\> | Original transfer target string (see above) |
| `transfer_target_type` | Option\<String\> | Resolved target kind (see above) |
| *+ctx* | | Flat context fields |

#### consult_switched

Dispatch: broadcast

Emitted when the talking party flips between customer and consult target
during an owner-anchored consultative transfer.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `transfer_id` | String | Transfer transaction ID |
| `talking_to` | String | Current talking party: `customer` \| `consult` |
| *+ctx* | | Flat context fields |

#### conference_auth_result

Dispatch: broadcast

Customer's DTMF response to the conference-authorization IVR
(`conference_auth` flow).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `transfer_id` | String | Transfer transaction ID |
| `result` | String | `authorized` \| `denied` \| `timeout` |
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

#### media_ringback_passthrough_started

Dispatch: call_owner

| Field | Type | Description |
|-------|------|-------------|
| `source` | String | Source leg call_id |
| `target` | String | Target leg call_id |

> Only the `started` event exists; no `stopped` event is emitted when the
> ringback pass-through ends (no such definition in code).

#### media_play_started / media_play_finished

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `leg_id` | Option\<String\> | Target leg |
| `track_id` | String | Playback track ID |
| `interrupted` | bool | `media_play_finished` only: whether interrupted by DTMF |
| *+ctx* | | Flat context fields |

#### dtmf

Dispatch: call_owner

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

#### record_started / record_paused / record_resumed

Dispatch: call_owner

> Trigger: Via `RecordStart` / `RecordPause` / `RecordResume` / `RecordStop` RWI commands. **Not automatic** — recording does not start automatically when a call is answered. There is no `record_failed` event — start/stop failures are returned as command errors.

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
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

> **Segmented recording**: each recording segment of a call (IVR slice, agent
> slice, …) emits **its own** event once uploaded — `filename` /
> `download_url` / `file_size` describe that segment only, and `extra`
> carries `seq` (per-call recording counter), `label` (agent id or IVR name),
> `segment_type`, `segment_id`, `started_at` / `ended_at` next to the
> call-level metadata. `record_end` remains a single per-call summary.
>
> **Backwards compatibility**: the pre-existing **aggregate event is still
> emitted once per call** (N segments → N+1 events) — its
> `extra.recording_segments` remains a JSON **string** (containing the array;
> `JSON.parse` it), and its `filename` is the first segment's file. Old
> subscribers keep working; consumers that only want per-segment events can
> skip the aggregate one (the event whose `extra` contains the
> `recording_segments` key). The CDR's `metadata.recording_segments` stays a
> native JSON array, unchanged.

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

RWI WebSocket frame (flat payload, `event_type` injected by the gateway):

```json
{
  "event_type": "recording_metadata_available",
  "call_id": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40",
  "metadata": {
    "filename": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40_02_1001.wav",
    "file_size": 153344,
    "download_url": "./config/recorders/20260910/0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40_02_1001.wav",
    "caller_name": "330909",
    "callee_name": "1001",
    "call_type": "inbound",
    "call_start_time": "2026-09-10T08:54:01.155781+00:00",
    "call_end_time": "2026-09-10T08:54:48.155781+00:00",
    "upload_time": "2026-09-10T08:54:18.157941+00:00",
    "session_id": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40",
    "segment_id": "9c1f02ab",
    "queue_id": "support",
    "label": "1001",
    "agent_name": "Agent 1001",
    "agent_id": "1001",
    "started_at": "2026-09-10T08:54:18.155781+00:00",
    "segment_type": "agent",
    "seq": "2",
    "ended_at": "2026-09-10T08:54:46.155781+00:00"
  }
}
```

Webhook delivery wraps the same payload in an envelope (`webhook.rs`: `rwi` /
`event_id` idempotency key / `timestamp` / nested `event`):

```json
{
  "rwi": "1.0",
  "event_id": "6b1f0a44-2f0e-4a3e-8e5d-9c7b1d2e3f45",
  "timestamp": "2026-09-10T08:54:18.312004+00:00",
  "call_id": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40",
  "event_type": "recording_metadata_available",
  "event": {
    "call_id": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40",
    "metadata": {
      "filename": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40_02_1001.wav",
      "file_size": 153344,
      "download_url": "./config/recorders/20260910/0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40_02_1001.wav",
      "caller_name": "330909",
      "callee_name": "1001",
      "call_type": "inbound",
      "call_start_time": "2026-09-10T08:54:01.155781+00:00",
      "call_end_time": "2026-09-10T08:54:48.155781+00:00",
      "upload_time": "2026-09-10T08:54:18.157941+00:00",
      "session_id": "0b7e6f4c-5b58-4a1e-9d2f-c3a8b19e7d40",
      "segment_id": "9c1f02ab",
      "queue_id": "support",
      "label": "1001",
      "agent_name": "Agent 1001",
      "agent_id": "1001",
      "started_at": "2026-09-10T08:54:18.155781+00:00",
      "segment_type": "agent",
      "seq": "2",
      "ended_at": "2026-09-10T08:54:46.155781+00:00"
    }
  }
}
```

> The examples above are the actual serialized output of the agent segment
> (`cargo test segment_metadata_wire_shape -- --nocapture`): the seq inside
> `filename` is zero-padded to two digits (`_02_`); every `extra` value is a
> string (`seq` serializes as `"2"`); `extra` key order is unspecified
> (HashMap); typed fields that are `None` are omitted entirely (e.g. no
> `caller_name`/`callee_name` when the CDR carries no SIP parties).
> `download_url`: `type=local` yields the archive path
> (`{path}/{YYYYMMDD}/{filename}`); `type=http`/`s3` yield the upload /
> preconstructed URL. Addon pass-through keys (wholesale `switch_flag`, …)
> are appended verbatim; there is no typed `unique_id` field. Without
> segmented recording (whole-call recording / SipFlow) the event keeps its
> legacy single-event shape and `metadata` carries no `seq` / `label` /
> `segment_*` keys.

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
>
> **Single completion event**: each step — including waiting steps (playback, digit collection, transfer awaiting result) — emits exactly **one** trace entry upon completion. The `trigger` keeps the step's original trigger source (e.g. `phone_collected`, `dtmf`) with its detail; a non-null `step_end_time` marks completion. No intermediate or duplicate events are emitted.
>
> **Exactly-once lifecycle contract**: within one logical IVR flow (including voip_bridge round-trips, queue returns, and JumpIvr jumps), `trigger.type="session_start"` and `trigger.type="session_end"` each appear **exactly once**:
> - `session_start` only on the first node's trace entry at the flow's true first entry;
> - resumable hand-offs (voip_bridge, queue return, JumpIvr) do **not** trigger `session_end` (the flow has not ended);
> - if the caller hangs up or the successor fails to start while the flow is suspended, the proxy synthesizes the single compensating `session_end` (`end_reason=user_hangup` / `error`; node context from the bridge trace context);
> - when the flow resumes after suspension, the resumed first node carries a `resume` trigger (no buffered digits) or `dtmf` (buffered digits) — never a second `session_start`.

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
| `duration_ms` | u64 | Step execution duration (ms), always present |
| `error` | Option\<String\> | Error message |
| `step_id` | Option\<String\> | Current node ID, returned by provider via ActionNode.step_id |
| `step_name` | Option\<String\> | Current node name, returned by provider via ActionNode.step_name |
| `step_start_time` | Option\<String\> | Current step start time (ISO UTC). Present on regular steps; null on derived entries (`session_end`, fallback, bridge DTMF) |
| `step_end_time` | Option\<String\> | Current step end time (ISO UTC), always present — it marks step completion (i.e. the event has been emitted) |
| `extra` | Option\<JSON Object\> | Transparent passthrough data from provider. Provider returns the complete object in ActionNode.extra each time; RustPBX stores and outputs it as-is |
| `sip_headers` | Option\<Map\<String, String\>\> | Whitelisted SIP headers of the call |
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
> | `type` | String | Trigger source type: `session_start`, `session_end`, `resume`, `dtmf`, `dtmf_menu`, `dtmf_menu_timeout`, `audio_complete`, `action_execute`, `chained`, `api_response`, `phone_collected`, `recording_complete`, `input_voice`, `error`, `dtmf_menu_invalid`, `unknown`. `resume` marks a flow resuming from a bridge/queue/JumpIvr suspension without buffered digits |
> | `detail` | Option\<JSON Object\> | Structured trigger detail, omitted when none. Common values: DTMF → `{"digit":"2"}`; API response → `{"status":200}`; phone collection → `{"number":"13800138000"}` |
>
> **Timing fields**:
> - `step_start_time` — when the current step started (previous step end or session start)
> - `step_end_time` — when the step ended; present on every entry (completion marker)
>
> **Duration fields**:
> - `duration_ms` — step execution duration (ms), always present, includes provider round-trip and action execution time

---

### 6.6 Queue / ACD Events

> **Event origin**: Queue-related events come in two families, produced by different subsystems and may co-occur:
> - **`queue_*` (queue lifecycle)**: produced by the Queue app (`src/call/app/queue.rs`, via `gw.broadcast`) **and** the CC ACD engine bridge (`src/addons/cc/mod.rs`, via `broadcast_event`). Covers the generic lifecycle: join, ringing, connected, abandon, timeout, fallback. Both subsystems dispatch as broadcast (the only exception is the `queue.enqueue` RWI command path, which answers to the owner).
> - **`skill_group_*` (skill-group scheduling decisions)**: produced **exclusively** by the CC addon's ACD adapter (`src/addons/cc/agent_registry_adapter.rs`) when the queue asks the ACD for an agent. Fires only when the CC addon is active and skill routing is used. The ACD-engine `queue_*` bridge intentionally does **not** emit `skill_group_*` (single source, no duplicates).
>
> Typical event sequence for a skill-group-routed call:
> `queue_joined` → `skill_group_candidates_found` → `skill_group_call_queued` (only when no agent is immediately available) → `skill_group_agent_assigned` → `queue_agent_offered` → `queue_agent_connected`
>
> `skill_group_call_abandoned` fires when the caller hangs up while still queued; `skill_group_service_unavailable` fires on queue timeout or fallback. Both are reported by the Queue app through the `AgentRegistry` lifecycle hooks (`notify_call_abandoned` / `notify_call_timeout` / `notify_call_fallback`), which the CC adapter maps to the RWI events.

All queue events carry flat context fields.

> **session_id correlation (since 2026-08)**: all call-scoped events
> (`queue_*` / `skill_group_*` / `call_*`) are enriched from
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

Dispatch: broadcast (relayed by the CC ACD bridge; the core Queue app does not emit it separately)

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `position` | usize | Current queue position |
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

> Voicemail redirection after wait-timeout has no dedicated event — it is
> expressed as `queue_fallback_executed` with `action = "voicemail"`. The
> `queue_voicemail_redirected` event listed in earlier revisions of this
> document does not exist.

#### queue_candidates_found

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `candidates` | Vec\<String\> | Candidate agent list |
| *+ctx* | | Flat context fields |

#### queue_agent_offered

Dispatch: broadcast

> Formerly emitted as `queue_agent_ringing` by the ACD bridge; the duplicate
> name was consolidated — one agent ring is always reported as
> `queue_agent_offered` regardless of the driver (built-in queue app or ACD
> engine).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `agent_id` | String | Agent ID |
| *+ctx* | | Flat context fields |

#### queue_agent_no_answer / queue_agent_rejected

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `agent_id` | String | Agent ID |
| `attempt` | u32 | Attempt number |
| *+ctx* | | Flat context fields |

#### queue_fallback_executed

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `queue_id` | String | Queue ID |
| `action` | String | Fallback action executed |
| `reason` | String | Reason |
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
| `reason` | String | `no_agent_available` \| `all_busy` \| `skill_mismatch` \| `capacity_full` |
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
| *+ctx* | | Flat context fields |

#### skill_group_service_unavailable

Dispatch: broadcast

Emitted when a queued call could not be serviced (queue timeout or fallback).

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `skill_group_id` | String | Skill group ID |
| `reason` | String | `exhausted_retries` \| `no_matching_skill` \| `overflow_chain_end` \| `schedule_off_hours` \| `timeout` |
| `attempts` | u32 | Retry attempts |
| `waited_secs` | u64 | Time waited |
| `fallback_action` | String | Executed fallback action |
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

#### agent_registered / agent_unregistered

Dispatch: broadcast

Emitted when an agent signs in / out (registry write or SIP registration bridge).

**agent_registered**:

| Field | Type | Description |
|-------|------|-------------|
| `agent_id` | String | Agent ID |
| `agent_name` | Option\<String\> | Agent display name |
| `agent_extension` | Option\<String\> | Bound extension |
| `team_id` | Option\<String\> | Team ID |

**agent_unregistered**:

| Field | Type | Description |
|-------|------|-------------|
| `agent_id` | String | Agent ID |
| `agent_name` | Option\<String\> | Agent display name |
| `reason_code` | Option\<String\> | Logout reason code |

#### presence_state_changed

Dispatch: broadcast

SIP PUBLISH presence state change (emitted on every local PUBLISH).

| Field | Type | Description |
|-------|------|-------------|
| `identity` | String | Presence identity (extension / AOR) |
| `from_status` | String | Previous status |
| `to_status` | String | New status |
| `note` | Option\<String\> | Optional note |
| `agent_id` | Option\<String\> | Associated agent ID, when resolvable |

---

### 6.8 Conference Events

#### conference_created / conference_destroyed

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference room ID |

#### conference_joined / conference_left

Dispatch: call_owner

Emitted when a member dials into (or leaves) a conference room via the
conference application (`conference:` target). Distinct from the
`conference_member_*` family, which is produced by conference control
commands (mute, kick, …).

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `call_id` | String | Member call ID |
| `leg_id` | String | Member leg |

> `conference_left` currently has a type definition but **no emission point**
> (reserved); `conference_joined` is emitted when a member joins.

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

#### conference_ended_by_host

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `host_call_id` | String | Host call ID |
| `removed_call_ids` | Vec\<String\> | Removed member call IDs |
| *+ctx* | | Flat context fields |

> No `conference_auto_ended` event exists (earlier revisions of this document
> were incorrect); conference teardown by the host is expressed by this event.

#### conference_error

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `error` | String | Error message |

> The consult flow does not emit `conference_consult_dialing` /
> `conference_consult_connected` (earlier revisions were incorrect); the real
> event is `consult_switched` (§6.2).

#### conference_merge_requested / conference_merged / conference_merge_failed

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call ID (`merge_requested` includes `consultation_call_id`) |
| `conf_id` | String | Conference ID (`merged`/`merge_failed`) |
| `consultation_call_id` | String | `merge_requested` only: consultation call ID |
| `reason` | String | `merge_failed` only: failure reason |
| *+ctx* | | Flat context fields |

#### conference_seat_replace_started / ...succeeded / ...failed

Dispatch: broadcast

| Field | Type | Description |
|-------|------|-------------|
| `conf_id` | String | Conference ID |
| `old_call_id` | String | Old member call ID |
| `new_call_id` | String | New member call ID |
| `reason` | String | `failed` only: failure reason |

**Seat replacement event sequence (success path)**:
1. `conference_seat_replace_started`
2. `conference_member_left` (old member leaves)
3. `conference_member_joined` (new member joins)
4. `conference_seat_replace_succeeded`

> No `conference_seat_replace_rollback_failed` event exists (earlier revisions
> were incorrect); the family has exactly started/succeeded/failed.

---

### 6.9 Supervisor Events

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

### 6.10 SIP Signaling Events

#### sip_message_received / sip_notify_received

| Field | Type | Description |
|-------|------|-------------|
| `call_id` | String | Call identifier |
| `content_type` | String | Content type |
| `body` | String | Message body |
| `event` | String | `sip_notify_received` only: SIP Event header |
| *+ctx* | | Flat context fields |

---

### 6.11 Session System Events

> No `call_ownership_changed` event exists (earlier revisions of this document
> were incorrect); call ownership / takeover is expressed through supervisor
> commands and their `supervisor_*_started` events.

#### session.resume / call.resume (command results, not events)

| Field | Type | Description |
|-------|------|-------------|
| `replayed_count` | u64 | Number of replayed cached events |
| `events` | array | Replayed entries (`timestamp` / `call_id` / `event`) |

---

## 7. Event Quick Reference

| Event Type | Dispatch | call_id | Context |
|------------|----------|---------|---------|
| `call_created` | owner | yes | own fields (inbound INVITE & originate) |
| `call_ringing` | owner | yes | +ctx (+`early_media`; one event per provisional response; carries agent_id when an agent is involved — there is no separate `call_early_media` event) |
| `call_answered` | owner | yes | +ctx |
| `call_bridged` | owner | leg_a | — |
| `call_unbridged` | owner | yes | +ctx |
| `call_transferred` | owner | yes | +ctx |
| `call_transfer_accepted` | owner | yes | +ctx |
| `call_transfer_failed` | owner | yes | +ctx |
| `consult_switched` | broadcast | yes | +ctx |
| `conference_auth_result` | broadcast | yes | +ctx |
| `call_hangup` | owner | yes | +ctx (+`duration_secs`) |
| `call_no_answer` | owner | yes | +ctx |
| `call_busy` | owner | yes | +ctx |
| `call_held` | owner | yes | +ctx |
| `call_unheld` | owner | yes | +ctx |
| `media_hold_started` | owner | yes | +ctx |
| `media_hold_stopped` | owner | yes | +ctx |
| `media_ringback_passthrough_started` | owner | yes | — |
| `media_play_started` | owner | yes | +ctx |
| `media_play_finished` | owner | yes | +ctx |
| `record_started` | owner | yes | +ctx |
| `record_paused` | owner | yes | +ctx |
| `record_resumed` | owner | yes | +ctx |
| `record_stopped` | owner | yes | own fields + enrich |
| `recording_metadata_available` | owner | yes | own fields + enrich (segmented recording: one per segment + one legacy aggregate per call whose extra carries the `recording_segments` JSON string) |
| `transcript_started` | owner | yes | own fields |
| `transcript_segment` | owner | yes | own fields + enrich |
| `transcript_error` | owner | yes | own fields + enrich |
| `transcript_ended` | owner | yes | own fields |
| `dtmf` | owner | yes | +ctx |
| `dtmf_collected` | owner | yes | +ctx |
| `dtmf_collection_timeout` | owner | yes | +ctx |
| `ivr_node_entered` | fan_out | yes | +ctx |
| `ivr_node_exited` | fan_out | yes | +ctx |
| `ivr_flow_completed` | fan_out | yes | +ctx |
| `ivr_step_trace` | fan_out | yes | — |
| `queue_joined` | owner/broadcast | yes | +ctx |
| `queue_position_changed` | broadcast | yes | +ctx |
| `queue_agent_offered` | broadcast | yes | +ctx |
| `queue_agent_connected` | broadcast | yes | +ctx |
| `queue_left` | broadcast | yes | +ctx |
| `queue_wait_timeout` | broadcast | yes | +ctx |
| `queue_candidates_found` | broadcast | yes | +ctx |
| `queue_agent_offered` | broadcast | yes | +ctx |
| `queue_agent_no_answer` | broadcast | yes | +ctx |
| `queue_agent_rejected` | broadcast | yes | +ctx |
| `queue_fallback_executed` | broadcast | yes | +ctx (voicemail redirection is `action = "voicemail"`, no dedicated event) |
| `queue_alert` | broadcast | — | — |
| `skill_group_candidates_found` | broadcast | yes | +ctx |
| `skill_group_agent_assigned` | broadcast | yes | +ctx |
| `skill_group_no_agent` | broadcast | yes | +ctx |
| `skill_group_call_queued` | broadcast | yes | +ctx |
| `skill_group_call_abandoned` | broadcast | yes | +ctx |
| `skill_group_service_unavailable` | broadcast | yes | +ctx |
| `agent_state_changed` | broadcast | optional | +ctx |
| `agent_registered` | broadcast | — | — |
| `agent_unregistered` | broadcast | — | — |
| `presence_state_changed` | broadcast | — | — |
| `call_held` | owner | yes | +ctx |
| `call_unheld` | owner | yes | +ctx |
| `conference_created` | broadcast | — | — |
| `conference_joined` | owner | yes | +ctx |
| `conference_left` | — (defined, not emitted) | yes | +ctx |
| `conference_member_joined` | broadcast | yes | +ctx |
| `conference_member_left` | broadcast | yes | +ctx |
| `conference_member_muted` | broadcast | yes | +ctx |
| `conference_member_unmuted` | broadcast | yes | +ctx |
| `conference_destroyed` | broadcast | — | — |
| `conference_ended_by_host` | broadcast | — | +ctx |
| `conference_error` | broadcast | — | — |
| `conference_merge_requested` | broadcast | yes | +ctx |
| `conference_merged` | broadcast | yes | +ctx |
| `conference_merge_failed` | broadcast | yes | +ctx |
| `conference_seat_replace_started` | broadcast | yes | — |
| `conference_seat_replace_succeeded` | broadcast | yes | — |
| `conference_seat_replace_failed` | broadcast | yes | — |
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
