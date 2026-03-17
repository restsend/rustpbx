# RustPBX WebSocket Interface (RWI) Proposal

RWI is a JSON-over-WebSocket control plane for RustPBX call orchestration.
It follows the action/event model of Asterisk AMI, adapted to JSON over WebSocket for modern contact center workflows with finer per-call control and simpler integration.

Design alignment with Asterisk AMI:
- Uniform snake_case for all keys: `action`, `action_id`, `event`, `response`
- Events follow AMI's async event push model
- Authentication uses a static AMI token in the HTTP header — no `login` action required

## 1. Goals

1. Full call lifecycle control over WebSocket.
2. Reuse RustPBX CallApp architecture (`external app` style integration).
3. Provide practical contact-center features out of the box:   
    - originate + bridge
    - inbound call decisioning (ring/reject/answer/play/transfer)
    - conference / three-way control
    - recording control
    - queue control
    - whisper / barge
    - optional PCM stream ingest/egress
4. Support both single-call and multi-call control models.

## 2. Scope

RWI provides:
- command channel: client -> RustPBX
- event channel: RustPBX -> client
- optional media channel: PCM binary frames over WebSocket

RWI does not replace SIP signaling. It controls call behavior through RustPBX internal `CallController` and `SessionAction` abstractions.

## 3. Architecture and CallApp Reuse

RWI should be implemented as an addon that creates a `CallApp` adapter (`RwiApp`) and maps WebSocket commands to `CallController` operations.

Existing internals that can be reused:
- `CallApp` lifecycle hooks (`on_enter`, `on_dtmf`, `on_external_event`, `on_timeout`, `on_exit`)
- `CallController` APIs (`answer`, `hangup`, `transfer`, `play_audio`, timeout/event flow)
- proxy-side session action plumbing (`SessionAction`)

Proposed high-level components:
- `RwiGateway`:
  - maintains authenticated WS sessions
  - routes commands/events by `call_id`
- `RwiApp` (per call):
  - bridges CallApp events to RWI events
  - executes validated RWI commands on `CallController`
- `RwiMediaBridge` (optional):
  - handles PCM stream subscription and injection

## 4. Protocol

### 4.1 Authentication

Authentication is performed at WebSocket upgrade time via HTTP header. No `Login` action is needed after the connection is established.

```
GET /rwi/v1 HTTP/1.1
Upgrade: websocket
Authorization: Bearer <ami_token>
```

Alternatively, the token may be passed as a query parameter for clients that cannot set headers:

```
GET /rwi/v1?token=<ami_token> HTTP/1.1
Upgrade: websocket
```

If the token is missing or invalid, the server rejects the upgrade with `HTTP 401 Unauthorized`.

AMI tokens are configured statically in `[rwi.tokens]` (see Section 14). Each token carries a set of permission scopes.

### 4.2 Envelope

All fields use snake_case consistently:

| Field       | Direction      | Purpose                           |
|-------------|----------------|-----------------------------------|
| `action`    | client → server| Command name                      |
| `action_id` | client → server| Correlation ID (client-generated) |
| `event`     | server → client| Async push event name             |
| `response`  | server → client| `success` or `error`              |

Client command:

```json
{
  "rwi": "1.0",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "params": {
    "call_id": "c_92f4"
  }
}
```

Success response:

```json
{
  "rwi": "1.0",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "response": "success",
  "data": {
    "call_id": "c_92f4"
  }
}
```

Error response:

```json
{
  "rwi": "1.0",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "response": "error",
  "error": {
    "code": "invalid_state",
    "message": "call is not in ringing state"
  }
}
```

Server event (async, no `action_id`):

```json
{
  "rwi": "1.0",
  "event": "call.incoming",
  "call_id": "c_92f4",
  "ts": "2026-03-13T09:15:22Z",
  "data": {
    "caller": "1001",
    "callee": "2000",
    "direction": "inbound"
  }
}
```

### 4.3 Command Idempotency

For commands that can be retried (`originate`, `bridge`, `transfer`, `record.start`), client should provide:

```json
"meta": { "idempotency_key": "<uuid>" }
```

Server caches result for a short TTL (recommended 60 seconds).

## 5. Core Commands

### 5.1 Originate and Bridge

Key params for `call.originate`:

| Param | Type | Description |
|---|---|---|
| `call_id` | string | Client-assigned leg identifier |
| `destination` | string | SIP URI to dial |
| `caller_id` | string | Caller ID to present |
| `timeout_secs` | int | Ring timeout |
| `hold_music` | object | Audio to play to the other leg while this leg is connecting |
| `ringback` | string | `"local"` (generate locally) \| `"passthrough"` (forward 183 early media from callee) \| `"none"` |
| `extra_headers` | object | Custom SIP headers to include in INVITE |

`hold_music` object:

```json
{ "type": "file", "uri": "sounds/hold.wav", "loop": true }
{ "type": "silence" }
{ "type": "ringback" }   // alias for ringback passthrough
```

1) Originate A-leg:

```json
{
  "action": "call.originate",
  "action_id": "req-001",
  "params": {
    "call_id": "leg_a",
    "destination": "sip:bob@local",
    "caller_id": "4000",
    "timeout_secs": 30,
    "extra_headers": {
      "X-Campaign-ID": "camp_001"
    }
  }
}
```

2) Originate B-leg (with hold music played to leg_a while waiting):

```json
{
  "action": "call.originate",
  "action_id": "req-002",
  "params": {
    "call_id": "leg_b",
    "destination": "sip:13800138000@trunk/main",
    "caller_id": "4000",
    "timeout_secs": 45,
    "hold_music": { "type": "file", "uri": "sounds/hold.wav", "loop": true },
    "hold_music_target": "leg_a"
  }
}
```

`hold_music_target` specifies which already-answered leg receives the hold audio while the new outbound leg is connecting. When the new leg answers (or is hung up), hold music stops automatically.

3) Bridge:

```json
{
  "action": "call.bridge",
  "action_id": "req-003",
  "params": {
    "leg_a": "leg_a",
    "leg_b": "leg_b"
  }
}
```

### 5.2 Inbound Call Control

- `call.ring`
- `call.reject`
- `call.answer`
- `call.hold`
- `call.unhold`
- `media.play`
- `call.transfer`
- `call.transfer.attended` (keep original call, create consult call)
- `call.transfer.complete` (complete the transfer)
- `call.transfer.cancel` (cancel the transfer)
- `call.hangup`

Example: reject inbound call

```json
{
  "action": "call.reject",
  "action_id": "req-010",
  "params": {
    "call_id": "c_92f4",
    "reason": "busy"
  }
}
```

Example: play prompt

```json
{
  "action": "media.play",
  "action_id": "req-011",
  "params": {
    "call_id": "c_92f4",
    "source": { "type": "file", "uri": "sounds/welcome.wav" },
    "interrupt_on_dtmf": true
  }
}
```

Example: call.transfer with REFER result events:

```json
{
  "action": "call.transfer",
  "action_id": "req-012",
  "params": {
    "call_id": "c_92f4",
    "target": "sip:agent_2@local"
  }
}
```

Transfer result events (async, after REFER):

```json
{ "event": "call.transfer.accepted", "call_id": "c_92f4" }
{ "event": "call.transfer.failed",   "call_id": "c_92f4", "data": { "sip_status": 503 } }
```

### 5.3 Parallel Outbound Dial with Hold Music / Ringback Passthrough

This is the canonical contact-center "find first available agent" and "predictive outbound" pattern.

#### Scenario: outbound call with parallel agent hunt

```
Step 1 — originate leg_a (customer side), wait for answer
Step 2 — originate leg_b + leg_c in parallel while playing hold music to leg_a
Step 3 — first leg to answer wins; cancel the other
Step 4 — bridge leg_a ↔ winner
```

**Variant A: play hold music to leg_a while hunting**

```json
// Step 1: originate leg_a
{ "action": "call.originate", "action_id": "r1",
  "params": { "call_id": "leg_a", "destination": "sip:bob@local", "caller_id": "4000" } }

// → event: call.answered, call_id: leg_a
// (bob is now on hold, waiting for audio)

// Step 2a: start hold music on leg_a
{ "action": "media.play", "action_id": "r2",
  "params": { "call_id": "leg_a",
               "source": { "type": "file", "uri": "sounds/hold.wav", "loop": true } } }

// Step 2b: originate leg_b and leg_c in parallel (no hold_music_target needed,
//          leg_a is already playing hold music independently)
{ "action": "call.originate", "action_id": "r3",
  "params": { "call_id": "leg_b", "destination": "sip:alice@local",
               "caller_id": "4000", "timeout_secs": 30 } }
{ "action": "call.originate", "action_id": "r4",
  "params": { "call_id": "leg_c", "destination": "sip:alice_0@local",
               "caller_id": "4000", "timeout_secs": 30 } }

// → event: call.answered, call_id: leg_c   (alice_0 answered first)

// Step 3: cancel leg_b
{ "action": "call.hangup", "action_id": "r5",
  "params": { "call_id": "leg_b" } }

// Step 4: stop hold music on leg_a, then bridge
{ "action": "media.stop", "action_id": "r6",
  "params": { "call_id": "leg_a" } }
{ "action": "call.bridge", "action_id": "r7",
  "params": { "leg_a": "leg_a", "leg_b": "leg_c" } }

// → event: call.bridged
```

**Variant B: passthrough ringback from callee to leg_a**

Instead of local hold music, forward the 183 early media (ringback tone) from one of the outbound legs to leg_a. Only one outbound leg's ringback can be forwarded at a time.

```json
// Step 2: originate leg_b with ringback passthrough to leg_a
{ "action": "call.originate", "action_id": "r3",
  "params": { "call_id": "leg_b", "destination": "sip:alice@local",
               "caller_id": "4000", "timeout_secs": 30,
               "ringback": "passthrough",
               "ringback_target": "leg_a" } }

// leg_a now hears whatever the remote party's 183 early media carries
// (carrier ringback tone, custom IVR prompt, etc.)

// On answer:
// → event: call.answered, call_id: leg_b
// bridge immediately (no media.stop needed, bridge replaces the passthrough path)
{ "action": "call.bridge", "action_id": "r4",
  "params": { "leg_a": "leg_a", "leg_b": "leg_b" } }
```

**Variant C: parallel hunt with per-leg ringback passthrough rotation**

For parallel dialing you cannot passthrough ringback from both legs simultaneously. Recommended pattern:
- Choose one leg (e.g. leg_b) as the "ringback source" for leg_a
- If leg_b times out before leg_c answers, switch ringback source to leg_c via `call.set_ringback_source`
- Or fall back to local hold music on the first 183-less leg

```json
// Switch ringback source mid-flight
{ "action": "call.set_ringback_source", "action_id": "r5",
  "params": { "target_call_id": "leg_a", "source_call_id": "leg_c" } }
```

#### Hold music and ringback state machine

```
leg_a state       | what leg_a hears
──────────────────|─────────────────────────────────────────────────
originating       | silence (or local ringback toward leg_a itself)
answered, parked  | silence  ← RWI client must explicitly start media
media.play active | hold music / MOH file (loops until media.stop or bridge)
ringback passthru | 183 early media from source outbound leg
bridged           | live audio from the bridged party
```

### 5.4 SIP In-Dialog Messages

**Supported outbound in-dialog actions:**

| Action | Underlying SIP | Use case |
|---|---|---|
| `sip.message` | SIP MESSAGE | Send in-call text (e.g. URL push, agent note) |
| `sip.notify` | SIP NOTIFY | Send event notification to remote endpoint |
| `sip.options_ping` | SIP OPTIONS | Keepalive / capability probe on an active call |

Example: send SIP MESSAGE within an active call:

```json
{
  "action": "sip.message",
  "action_id": "req-msg-01",
  "params": {
    "call_id": "c_92f4",
    "content_type": "text/plain",
    "body": "Your ticket number is 12345"
  }
}
```

Example: send SIP NOTIFY (e.g. MWI or custom event):

```json
{
  "action": "sip.notify",
  "action_id": "req-ntf-01",
  "params": {
    "call_id": "c_92f4",
    "event": "check-sync",
    "content_type": "application/simple-message-summary",
    "body": "Messages-Waiting: yes"
  }
}
```

**Inbound in-dialog messages** received on active calls are delivered as RWI events:

```json
{
  "event": "sip.message.received",
  "call_id": "c_92f4",
  "data": {
    "content_type": "text/plain",
    "body": "Customer replied: I need billing help"
  }
}
```

Notes:
- Out-of-dialog standalone SIP MESSAGE (presence state, IM to a URI) is **not** handled here — that belongs to `/ami/v1` or a separate SIP messaging addon.
- `sip.options_ping` does not support response body inspection — use `/ami/v1/health` for endpoint reachability checks.
- All three actions require the call to be in an active (answered) state.

## 6. Conference / Three-Way (Addon)

Three-way is implemented via conference addon primitives.

Commands:
- `conference.create`
- `conference.add`
- `conference.remove`
- `conference.mute`
- `conference.destroy`

Three-way flow:
1. active call between agent and customer
2. `conference.create(conf_1)`
3. `conference.add(conf_1, call_agent_customer)`
4. `call.originate(third_party)`
5. `conference.add(conf_1, call_third_party)`

## 7. Recording Requirements (New)

RWI should provide explicit recording commands to satisfy compliance and QA needs.

Commands:
- `record.start`
- `record.pause`
- `record.resume`
- `record.stop`
- `record.mask_segment` (optional, for PCI redaction windows)

Example:

```json
{
  "action": "record.start",
  "action_id": "req-020",
  "params": {
    "call_id": "c_92f4",
    "mode": "mixed",
    "beep": false,
    "max_duration_secs": 7200,
    "storage": {
      "backend": "local",
      "path": "records/2026/03/13/c_92f4.wav"
    }
  }
}
```

Events:
- `record.started`
- `record.paused`
- `record.resumed`
- `record.stopped`
- `record.failed`

Notes:
- `mode`: `mixed` or `separate_legs`
- store recording id for resume/stop correlation
- emit CDR correlation fields (`call_id`, `queue_id`, `agent_id`, `campaign_id`)

## 8. Queue Requirements (New)

RWI queue control should integrate with queue addon behavior rather than bypassing it.

Commands:
- `queue.enqueue`
- `queue.dequeue`
- `queue.hold`
- `queue.unhold`
- `queue.set_priority`
- `queue.assign_agent`
- `queue.requeue`

Example:

```json
{
  "action": "queue.enqueue",
  "action_id": "req-030",
  "params": {
    "call_id": "c_92f4",
    "queue_id": "support_l1",
    "priority": 5,
    "skills": ["billing", "zh"],
    "max_wait_secs": 300
  }
}
```

Queue events:
- `queue.joined`
- `queue.position_changed`
- `queue.wait_timeout`
- `queue.agent_offered`
- `queue.agent_connected`
- `queue.left`

Contact center practical requirements:
- expose estimated wait time
- expose queue position
- provide ring-no-answer outcomes per agent attempt
- support overflow routing policy event hints

## 9. Whisper / Barge Requirements (New)

Supervisor operations:
- `supervisor.listen` (silent monitor)
- `supervisor.whisper` (speak to agent only)
- `supervisor.barge` (join both sides)
- `supervisor.takeover` (optional: replace agent)

Example: whisper

```json
{
  "action": "supervisor.whisper",
  "action_id": "req-040",
  "params": {
    "supervisor_call_id": "sup_001",
    "target_call_id": "c_92f4",
    "agent_leg": "a_leg"
  }
}
```

Events:
- `supervisor.listen.started`
- `supervisor.whisper.started`
- `supervisor.barge.started`
- `supervisor.mode.stopped`

Implementation notes:
- requires media routing control at leg level
- strict RBAC: only supervisor role can execute
- emit audit logs for all monitor modes

## 10. PCM Stream Control

RWI supports optional PCM processing for AI/ASR bots.

Commands:
- `media.stream.start`
- `media.stream.stop`
- `media.inject.start`
- `media.inject.stop`

Example:

```json
{
  "action": "media.stream.start",
  "action_id": "req-050",
  "params": {
    "call_id": "c_92f4",
    "direction": "recv",
    "format": {
      "codec": "pcm_s16le",
      "sample_rate": 8000,
      "channels": 1,
      "ptime_ms": 20
    }
  }
}
```

If disabled by policy, server returns `MEDIA_STREAM_DISABLED`.

## 11. Multi-Client Session Model and Inbound Call Flow

### 11.1 Overview

Multiple RWI clients can connect to the same RWI gateway simultaneously. Each connection is independent and authenticated with its own token. A single client can manage many concurrent calls over one WebSocket connection.

Key concepts:
- **Context**: a named routing label that maps inbound calls to interested clients. Clients subscribe to contexts; inbound calls are dispatched by context.
- **Ownership**: each active call has exactly one controlling client at a time. Only the owner can issue control actions (`call.answer`, `media.play`, `call.hangup`, etc.).
- **Fan-out**: `call.incoming` is delivered to all clients subscribed to the matching context. Ownership is determined by first-claim.

### 11.2 Session Lifecycle

```
client connects (token validated at HTTP upgrade)
  │
  ├─► session.subscribe { contexts: ["ivr_bot", "queue_agent_1"] }
  │     → server: response success
  │
  ├─► [optional] call.originate  → owns the outbound call immediately
  │
  ├─► [inbound] call.incoming event arrives for subscribed context
  │     → client calls call.answer / call.reject to claim
  │
  └─► client disconnects
        → owned calls enter no_answer_action (hangup or hold, per context config)
```

Session commands:
- `session.subscribe` — subscribe to one or more contexts
- `session.unsubscribe` — remove context subscription
- `session.list_calls` — list calls currently owned by this session
- `session.attach_call` — attach to an already-active call (supervisor or takeover)
- `session.detach_call` — release ownership or supervision

Subscribe example:

```json
{
  "action": "session.subscribe",
  "action_id": "req-s01",
  "params": {
    "contexts": ["ivr_bot", "queue_overflow"]
  }
}
```

### 11.3 Inbound Call Dispatch Flow

Step-by-step when a SIP INVITE arrives:

```
 1. SIP INVITE → RustPBX proxy
 2. Dialplan routing resolves: app=rwi, context="ivr_bot"
 3. RustPBX creates RwiApp for the call, holds it in ringing state
 4. RwiGateway fans out call.incoming to ALL clients subscribed to "ivr_bot"
 5. Client(s) receive call.incoming and may call.answer / call.reject
 6. First valid call.answer wins → that client becomes owner
    └─ other clients receive call.answered event but lose control access
 7. If no client responds within no_answer_timeout_secs:
    └─ server executes no_answer_action (hangup, play tone, or transfer to fallback)
```

`call.incoming` event:

```json
{
  "rwi": "1.0",
  "event": "call.incoming",
  "call_id": "c_92f4",
  "context": "ivr_bot",
  "ts": "2026-03-14T09:15:22Z",
  "data": {
    "caller": "1001",
    "callee": "2000",
    "direction": "inbound",
    "trunk": "trunk_main",
    "sip_headers": {
      "X-Tenant-ID": "corp_a",
      "P-Asserted-Identity": "sip:1001@pbx.local",
      "X-Campaign-ID": "camp_001"
    }
  }
}
```

Note: `sip_headers` is **read-only** and contains only headers explicitly whitelisted in `[rwi.sip_header_passthrough]` config. Raw SIP is never fully exposed.

Race condition handling:
- First `call.answer` for a given `call_id` wins; subsequent attempts return `response: "error", code: "already_owned"`.
- Server serializes claim attempts per `call_id` to prevent split ownership.

### 11.4 Outbound Call Ownership

Calls originated via `call.originate` are owned by the originating client immediately — no subscribe or attach step is needed. All events for that call are delivered only to the owning client (and any supervisor clients).

### 11.5 Event Delivery Rules

| Client state                              | Events delivered                        |
|-------------------------------------------|-----------------------------------------|
| Subscribed to context, call not yet owned | `call.incoming` only                    |
| Owns the call                             | All call, media, record, queue events   |
| Subscribed but lost ownership race        | `call.answered` / `call.hangup` only    |
| Not subscribed, not owner                 | None                                    |
| Supervisor attached                       | Supervisor events + read-only call events|

### 11.6 Attach to Existing Call

A client can attach to an already-active call (e.g., supervisor monitor, or agent takeover after re-connect):

```json
{
  "action": "session.attach_call",
  "action_id": "req-060",
  "params": {
    "call_id": "c_92f4",
    "mode": "control"
  }
}
```

`mode` values:
- `control` — take ownership (requires original owner to have detached, or `supervisor.control` scope with force-takeover)
- `listen` — supervisor read-only event stream
- `whisper` — supervisor whisper mode
- `barge` — supervisor barge (join both legs)

## 12. Event Taxonomy

Call events:
- `call.incoming` — inbound call arrived (dispatched to subscribed context clients)
- `call.ringing` — remote party is ringing (outbound leg received 180)
- `call.early_media` — remote party sent 183 with SDP (early media / ringback passthrough active)
- `call.answered` — call answered (200 OK)
- `call.bridged` — two legs bridged together
- `call.unbridged` — bridge torn down (one leg hung up or explicit unbridge)
- `call.transferred` — transfer initiated via REFER
- `call.transfer.accepted` — REFER target accepted (200 on NOTIFY)
- `call.transfer.failed` — REFER target failed
- `call.hangup` — call ended (includes `reason` and `sip_status`)
- `call.no_answer` — outbound leg timed out with no answer
- `call.busy` — outbound leg returned 486 Busy

Hold / ringback events:
- `media.hold.started` — hold music started on a leg
- `media.hold.stopped` — hold music stopped (bridge imminent or explicit stop)
- `media.ringback.passthrough.started` — 183 early media is being forwarded to target leg
- `media.ringback.passthrough.stopped` — early media passthrough ended

SIP in-dialog events:
- `sip.message.received`
- `sip.notify.received`

Media events:
- `media.play.started`
- `media.play.finished`
- `media.stream.started`
- `media.stream.stopped`

Recording events:
- `record.started`
- `record.stopped`
- `record.failed`

Queue events:
- `queue.joined`
- `queue.position_changed`
- `queue.agent_connected`
- `queue.left`

Supervisor events:
- `supervisor.listen.started`
- `supervisor.whisper.started`
- `supervisor.barge.started`

## 13. Security and Governance

1. Authentication:
- Static AMI token passed as `Authorization: Bearer <ami_token>` HTTP header on WebSocket upgrade
- No `Login` action required after connection is established
- Token-less or invalid-token upgrades are rejected with `HTTP 401 Unauthorized` before the WebSocket handshake completes
- Tokens are configured statically in `[rwi.tokens]`; each token entry carries a fixed set of permission scopes
- Token rotation is done by updating config and reloading — no session re-auth needed

2. Authorization:
- per-token RBAC scopes (`call.control`, `queue.control`, `supervisor.control`, `media.stream`)
- per-call ownership checks

3. Safety:
- rate limit high-risk actions (`originate`, `transfer`, `barge`)
- protect against duplicate actions via idempotency keys
- audit trail for supervisor and recording actions

4. Transport:
- `wss` only in production
- optional mTLS for trusted control-plane clients

## 14. Suggested Config

```toml
[rwi]
enabled = true
listen = "0.0.0.0:8088"
max_connections = 2000
max_calls_per_connection = 200

# AMI tokens — no login action required.
# Each token entry defines a static bearer token and its permission scopes.
[[rwi.tokens]]
token = "secret-control-token"
scopes = ["call.control", "queue.control", "record.control"]

[[rwi.tokens]]
token = "secret-supervisor-token"
scopes = ["call.control", "supervisor.control", "media.stream"]

[[rwi.tokens]]
token = "secret-bot-token"
scopes = ["call.control", "media.stream"]

# Contexts define how inbound calls are dispatched to RWI clients.
# Dialplan routes a call with: app=rwi, context=<name>
# All clients subscribed to that context receive call.incoming.
[[rwi.contexts]]
name = "ivr_bot"
no_answer_timeout_secs = 10
no_answer_action = "hangup"          # hangup | transfer | play_tone

[[rwi.contexts]]
name = "queue_agent_1"
no_answer_timeout_secs = 30
no_answer_action = "transfer"
no_answer_transfer_target = "sip:voicemail@local"

[rwi.media]
enabled = true
max_media_streams = 500
allow_inject = true

# SIP headers whitelisted for passthrough in call.incoming / call.originate
[rwi.sip_header_passthrough]
inbound = ["X-Tenant-ID", "P-Asserted-Identity", "X-Campaign-ID", "X-Priority"]
outbound = ["X-Campaign-ID", "X-Priority", "X-Agent-ID"]

[rwi.supervisor]
enabled = true
require_audit = true

[rwi.queue]
enabled = true
```

## 15. MVP and Rollout

Phase P0:
- auth via AMI token header, `session.subscribe` / context dispatch
- inbound control (`call.incoming` fan-out, `call.answer/reject/hangup/transfer/play`)
- originate A/B + bridge
- basic events, ownership, and error model

Phase P1:
- recording (`start/stop`)
- queue enqueue/dequeue and queue events
- multi-call session attach/detach

Phase P2:
- whisper/listen/barge
- PCM inject + dedicated media WS
- advanced observability and SLA metrics

## 16. Why RWI is More Practical than ESL/AMI for Contact Center

1. JSON protocol and native WebSocket make client integration faster.
2. Fine-grained per-call control and attach model reduce accidental global operations.
3. Built-in queue, recording, and supervisor semantics match real call center workflows.
4. Optional PCM stream in the same ecosystem enables AI/ASR features without protocol fragmentation.

## 17. Agent Presence Pub/Sub

### 17.1 Problem

Contact center agents have state beyond individual calls: `available`, `busy`, `wrap_up`, `break`, `offline`. This state needs to be:
- published by agents or ACD logic
- subscribed to by supervisors, wallboards, and queue routing
- correlated with call events (e.g. auto-transition to `busy` on `call.answered`)

This is a **presence / pub-sub problem**, distinct from per-call control.

### 17.2 Design Choice: not inside RWI

Agent presence should **not** be embedded in RWI's call-control WebSocket for the following reasons:
- RWI sessions are authenticated by AMI token (system-level), not by agent identity
- Presence requires a per-agent identity model and roster management
- Presence may need to survive call-leg disconnects (agent state persists after WS reconnect)
- Mixing presence state with call control creates coupling that complicates both

### 17.3 Recommended Approach

Two viable options depending on scale:

**Option A: SIP SUBSCRIBE/PUBLISH (RFC 3856 presence)**
- Agents publish presence via SIP PUBLISH to RustPBX
- Supervisors/wallboards subscribe via SIP SUBSCRIBE + NOTIFY
- Pros: standards-based, works with existing SIP phones
- Cons: complex to implement, heavyweight for browser clients

**Option B: Dedicated presence REST + SSE endpoint (simpler, recommended for P1)**
- `POST /ami/v1/agents/{id}/state` — agent or ACD sets state
- `GET  /ami/v1/agents/events` — SSE stream of state changes (supervisor/wallboard)
- `GET  /ami/v1/agents` — current roster snapshot
- RustPBX auto-publishes `busy`/`wrap_up` transitions triggered by call events
- Pros: simple HTTP, easy to integrate with web dashboards
- Cons: not SIP-native

### 17.4 RWI Integration Point

RWI does carry one presence hint: when a call is answered/hungup, RWI can emit an agent state hint that the presence layer consumes:

```json
{ "event": "call.answered", "call_id": "c_92f4", "data": { "agent_id": "agent_42" } }
```

The presence service listens for these events and auto-transitions agent state. This keeps the two systems loosely coupled.

## 18. MCU / Conference Media

### 18.1 Is MCU Required?

No, MCU is **not required to be built-in**. The answer depends on the conference use case:

| Use case | MCU needed? | Approach |
|---|---|---|
| 3-way call (agent + customer + supervisor) | No | SIP B2BUA re-INVITE mixing (already in RustPBX) |
| Small conference ≤ 5 legs | No | SIP mixing at media proxy level |
| Large conference (> 5 legs, recording, transcription) | Yes | External MCU or media server |
| Webinar / broadcast (1→N) | Yes | External MCU required |

### 18.2 Built-in Approach (No MCU)

RustPBX's existing media proxy can do N-leg mixing for small conferences by chaining RTP streams. The `conference.create` / `conference.add` commands in Section 6 target this path. This is sufficient for:
- 3-way calls
- agent + supervisor barge
- small team rooms

### 18.3 External MCU Integration

For larger conferences, RWI should bridge to an external media server (e.g. Janus, FreeSWITCH conference module, mediasoup) rather than building one in.

Proposed integration pattern:
1. RWI client calls `conference.create` with `backend: "external"`
2. RustPBX originates a SIP call leg to the external MCU room URI
3. Other legs are bridged into the MCU via the same mechanism
4. RWI events (`conference.member.joined`, `conference.member.left`) are forwarded from the MCU via SIP NOTIFY or a webhook, then re-emitted as RWI events

```json
{
  "action": "conference.create",
  "action_id": "req-conf-01",
  "params": {
    "conf_id": "room_42",
    "backend": "external",
    "mcu_uri": "sip:room_42@mcu.internal",
    "max_members": 50,
    "record": true
  }
}
```

### 18.4 Summary

- **P0/P1**: no MCU needed — use built-in B2BUA mixing for ≤ 5-leg conferences
- **P2**: add external MCU bridge for large conference and webinar use cases
- Do not build a media server inside RustPBX — use SIP as the integration point to delegate media to a dedicated MCU
