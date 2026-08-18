# RustPBX WebSocket Interface (RWI)

RWI is a JSON-over-WebSocket control plane for RustPBX call orchestration. It follows Asterisk AMI's action/event model, adapted to JSON over WebSocket for modern contact center workflows.

## 1. Overview

RWI provides:
- **Command channel**: client → RustPBX
- **Event channel**: RustPBX → client

RWI does not replace SIP signaling. It controls call behavior through RustPBX internal `CallController` and `SessionAction` abstractions.

## 2. Architecture

RWI is implemented as a built-in RustPBX module with core components:

- **RwiGateway**: maintains authenticated WS sessions, routes commands/events by `call_id`
- **RwiApp** (per call): bridges CallApp events to RWI events, executes validated RWI commands on `CallController`

## 3. Authentication

Authentication is performed at WebSocket upgrade time via HTTP header. No `Login` action is required.

```
GET /rwi/v1 HTTP/1.1
Upgrade: websocket
Authorization: Bearer <ami_token>
```

Or pass token as query parameter (for clients that cannot set headers):

```
GET /rwi/v1?token=<ami_token> HTTP/1.1
Upgrade: websocket
```

If token is missing or invalid, server rejects upgrade with `HTTP 401 Unauthorized` before WebSocket handshake completes.

Tokens are configured statically in `[rwi.tokens]`. Each token carries a set of permission scopes.

## 4. Protocol

### 4.1 Envelope

All fields use snake_case consistently:

| Field | Direction | Purpose |
|-------|-----------|---------|
| `rwi` | both | Protocol version (optional, for future compatibility) |
| `action` | client → server | Command name (required) |
| `action_id` | client → server | Client-generated correlation ID (required) |
| `event` | server → client | Async push event name |

Note: The `rwi` field is optional and currently ignored. Version is already encoded in the WebSocket URL path (`/rwi/v1`).

### 4.2 Async Command Model

RWI uses a fully asynchronous event-driven model (similar to FreeSWITCH ESL). **All commands receive their results via events** - there are no synchronous responses.

**Command flow:**
1. Client sends command with `action_id`
2. Server validates and executes asynchronously
3. Server sends `command_completed` or `command_failed` event with matching `action_id`
4. Client correlates the response via `action_id`

**Client command format:**

```json
{
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "params": {
    "call_id": "c_92f4"
  }
}
```

**Command completed event:**

```json
{
  "type": "command_completed",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "call_id": "c_92f4",
  "status": "success"
}
```

**Command with data result:**

```json
{
  "type": "command_completed",
  "action_id": "req-001",
  "action": "call.originate",
  "status": "success",
  "data": {
    "call_id": "c_92f4"
  }
}
```

**Command failed event:**

```json
{
  "type": "command_failed",
  "action_id": "b0e31d3a-5f7c-4fd9-b987-f5ec7e7e5c49",
  "action": "call.answer",
  "call_id": "c_92f4",
  "error": "Call not found: c_92f4"
}
```

**Server async event (no action_id):**

```json
{
  "event": "call.incoming",
  "call_id": "c_92f4",
  "data": {
    "context": "default",
    "caller": "1001",
    "callee": "2000",
    "direction": "inbound",
    "trunk": "trunk_main",
    "sip_headers": {
      "X-Tenant-ID": "corp_a",
      "P-Asserted-Identity": "sip:1001@pbx.local"
    }
  }
}
```

### 4.3 Command Format

RWI commands use JSON tagged union format. The `action` field identifies the command type, and `params` contains command-specific parameters:

```json
{
  "action": "call.originate",
  "action_id": "req-001",
  "params": {
    "call_id": "leg_a",
    "destination": "sip:bob@local",
    "caller_id": "4000",
    "timeout_secs": 30
  }
}
```

Some commands support aliases for convenience:

| Primary Name | Alias |
|-------------|-------|
| `session.subscribe` | `Subscribe` |
| `call.originate` | `Originate` |
| `call.answer` | `Answer` |
| `media.play` | `MediaPlay` |

## 5. Command Reference

### 5.1 Session Commands

| Command | Description |
|---------|-------------|
| `session.subscribe` | Subscribe to one or more contexts |
| `session.unsubscribe` | Unsubscribe from contexts |
| `session.list_calls` | List calls owned by this session |
| `session.attach_call` | Attach to existing call (supervisor mode) |
| `session.detach_call` | Release call ownership or supervision |
| `session.resume` | Resume the event stream from a sequence number |

**Subscribe example:**

```json
{
  "action": "session.subscribe",
  "action_id": "req-s01",
  "params": {
    "contexts": ["ivr_bot", "queue_overflow"]
  }
}
```

### 5.2 Call Control Commands

| Command | Description |
|---------|-------------|
| `call.originate` | Initiate outbound call |
| `call.answer` | Answer call |
| `call.reject` | Reject call |
| `call.ring` | Send ringing |
| `call.hangup` | Hangup call |
| `call.bridge` | Bridge two calls |
| `call.unbridge` | Unbridge call |
| `call.transfer` | Transfer call (blind) |
| `call.transfer.replace` | Transfer by replacing the target dialog (RFC 3891 Replaces) |
| `call.transfer.attended` | Attended transfer (consult first) |
| `call.transfer.complete` | Complete attended transfer |
| `call.transfer.cancel` | Cancel attended transfer |
| `call.hold` | Hold call (with optional music) |
| `call.unhold` | Unhold call |
| `call.set_ringback_source` | Set ringback source |
| `call.set_var` / `call.get_var` | Set / read a call variable |
| `call.send_dtmf` | Send DTMF digits to the remote party |
| `dtmf.collect` | Collect a multi-digit DTMF string |
| `call.leg_add` / `call.leg_remove` | Dynamically add / remove a call leg |
| `call.app_start` / `call.app_stop` | Start / stop a call application on the session |
| `call.resume` | Resume receiving events for a call after re-attach |
| `app.chain` | Chain into another call application |

**Originate call:**

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

**Bridge two calls:**

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

**Reject call:**

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

Valid `reason` values: `busy`, `forbidden`, `not_found`

### 5.3 Media Commands

| Command | Description |
|---------|-------------|
| `media.play` | Play audio file |
| `media.stop` | Stop playback |
| `call.transfer` → `voip_bridge:` / `bridge:` | Bidirectional PCM16 over WebSocket (see below) |

**Real-time bidirectional PCM (voip_bridge):**

`media.stream_start` / `media.inject_start` were removed — the supported
mechanism for real-time bidirectional PCM is transferring the call (or a leg)
to a `voip_bridge:` / `bridge:` WebSocket endpoint. The PBX dials out to the
given `ws(s)://` URL and shuttles little-endian PCM16 binary frames both ways
(DTMF is relayed as JSON text frames). Works for **inbound and outbound
(originated) calls** — transfer the `caller` leg after `call_answered`:

```json
{
  "action": "call.transfer",
  "action_id": "req-012",
  "params": {
    "call_id": "c_92f4",
    "target": "voip_bridge:ws://media.example.com:9000/ws?samplerate=8000&codec=pcm"
  }
}
```

Optional query parameters: `samplerate` (default 8000), `codec`, `timeout_ms`,
`_hdr_<name>` (custom headers), `return_app` / `return_target` (where the call
returns if the bridge disconnects).

**Play audio:**

```json
{
  "action": "media.play",
  "action_id": "req-011",
  "params": {
    "call_id": "c_92f4",
    "source": {
      "type": "file",
      "uri": "sounds/welcome.wav"
    },
    "interrupt_on_dtmf": true
  }
}
```

**Media source types:**

```json
{ "type": "file", "uri": "sounds/hold.wav", "looped": true }
{ "type": "silence" }
{ "type": "ringback" }
```

### 5.4 Recording Commands

| Command | Description |
|---------|-------------|
| `record.start` | Start recording |
| `record.pause` | Pause recording |
| `record.resume` | Resume recording |
| `record.stop` | Stop recording |

**Start recording:**

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
      "path": "records/2026/03/13/c_92f4.wav"
    }
  }
}
```

`record.start` works on inbound **and outbound (originated) calls** — an
explicit start is honoured regardless of the dialplan auto-record flag. When
the recording finishes (explicit stop or call hangup) the owner receives
`record_stopped`, and the CDR carries the recording file (→ `recording_url`).
With `[recording]` enabled, `recording_metadata_available` and `record_end`
are also emitted.

**Recording on originate (auto-start on answer):** `call.originate` (and
`POST /ami/v1/outbound/dial`) accepts a `record` object with the same shape as
`record.start`; recording starts automatically once the call is answered.
An empty `storage.path` uses the default location
(`[recording].path/<call_id>.wav`):

```json
{
  "action": "call.originate",
  "action_id": "req-021",
  "params": {
    "call_id": "c_out1",
    "destination": "sip:1002@pbx.local",
    "caller_id": "sip:1000@pbx.local",
    "record": { "mode": "mixed", "beep": false, "storage": { "path": "" } }
  }
}
```

Valid `mode` values:

- `mixed`: mix both call directions into a one-channel WAV.
- `separate_legs`: write the two call directions as separate stereo channels.

### 5.5 Queue Commands

| Command | Description |
|---------|-------------|
| `queue.enqueue` | Add to queue |
| `queue.dequeue` | Remove from queue |
| `queue.hold` | Hold in queue |
| `queue.unhold` | Unhold from queue |
| `queue.set_priority` | Set priority |
| `queue.assign_agent` | Assign agent |
| `queue.requeue` | Re-queue |

**Enqueue:**

```json
{
  "action": "queue.enqueue",
  "action_id": "req-030",
  "params": {
    "call_id": "c_92f4",
    "queue_id": "support_l1",
    "priority": 5
  }
}
```

### 5.6 Supervisor Commands

| Command | Description |
|---------|-------------|
| `supervisor.listen` | Silent monitor |
| `supervisor.whisper` | Whisper to agent only |
| `supervisor.barge` | Join both sides |
| `supervisor.takeover` | Takeover (replace agent) |
| `supervisor.stop` | Stop supervisor mode |

**Whisper:**

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

### 5.7 SIP Message Commands

| Command | Description |
|---------|-------------|
| `sip.message` | Send SIP MESSAGE |
| `sip.notify` | Send SIP NOTIFY |
| `sip.options_ping` | SIP OPTIONS ping |

**Send SIP MESSAGE:**

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

### 5.8 Conference Commands

| Command | Description |
|---------|-------------|
| `conference.create` | Create conference |
| `conference.add` | Add call to conference |
| `conference.remove` | Remove call from conference |
| `conference.mute` | Mute participant |
| `conference.unmute` | Unmute participant |
| `conference.destroy` | Destroy conference |
| `conference.end` | End conference by host (kicks all participants) |
| `conference.merge` | Merge a consultation leg into a conference |
| `conference.seat_replace` | Replace one participant with another atomically |

**Create conference:**

```json
{
  "action": "conference.create",
  "action_id": "req-conf-01",
  "params": {
    "conf_id": "room_42",
    "max_members": 10
  }
}
```

**Seat replacement (A -> A1):**

```json
{
  "action": "conference.seat_replace",
  "action_id": "req-conf-seat-01",
  "params": {
    "conference_id": "room_42",
    "old_call_id": "call_a",
    "new_call_id": "call_a1"
  }
}
```

## 6. Event Reference

### 6.1 Command Result Events

| Event | Description |
|-------|-------------|
| `command_completed` | Command executed successfully (contains `action_id`, `action`, optional `data`) |
| `command_failed` | Command execution failed (contains `action_id`, `action`, `error`) |

**Command completed with data:**

```json
{
  "type": "command_completed",
  "action_id": "req-001",
  "action": "call.originate",
  "call_id": "c_92f4",
  "status": "success",
  "data": {
    "call_id": "c_92f4"
  }
}
```

### 6.2 Call Events

| Event | Description |
|-------|-------------|
| `call.incoming` | Inbound call arrived (dispatched to subscribed contexts) |
| `call.ringing` | Remote party is ringing (outbound leg received 180) |
| `call.early_media` | Remote sent 183 with SDP (early media/ringback passthrough active) |
| `call.answered` | Call answered (200 OK) |
| `call.bridged` | Two legs bridged together |
| `call.unbridged` | Bridge torn down |
| `call.transferred` | Transfer initiated via REFER |
| `call.transfer.accepted` | REFER target accepted |
| `call.transfer.failed` | REFER target failed |
| `call.hangup` | Call ended |
| `call.no_answer` | Outbound leg timed out |
| `call.busy` | Outbound leg returned 486 Busy |

### 6.3 Media Events

| Event | Description |
|-------|-------------|
| `media.hold.started` | Hold music started |
| `media.hold.stopped` | Hold music stopped |
| `media.ringback.passthrough.started` | 183 early media being forwarded to target leg |
| `media.play.started` | Playback started |
| `media.play.finished` | Playback finished |

### 6.4 Recording Events

| Event | Description |
|-------|-------------|
| `record.started` | Recording started |
| `record.paused` | Recording paused |
| `record.resumed` | Recording resumed |
| `record.stopped` | Recording stopped |
| `record.failed` | Recording failed |

### 6.5 Queue Events

| Event | Description |
|-------|-------------|
| `queue.joined` | Joined queue |
| `queue.position_changed` | Queue position changed |
| `queue.agent_offered` | Call offered to agent |
| `queue.agent_connected` | Agent connected |
| `queue.left` | Left queue |
| `queue.wait_timeout` | Wait timeout |

### 6.6 Supervisor Events

| Event | Description |
|-------|-------------|
| `supervisor.listen.started` | Listen started |
| `supervisor.whisper.started` | Whisper started |
| `supervisor.barge.started` | Barge started |
| `supervisor.mode.stopped` | Supervisor mode stopped |
| `supervisor.takeover.started` | Supervisor took over the agent leg |

### 6.7 SIP Events

| Event | Description |
|-------|-------------|
| `sip.message.received` | SIP MESSAGE received |
| `sip.notify.received` | SIP NOTIFY received |
| `dtmf` | DTMF digit |

### 6.8 Conference Events

| Event | Description |
|-------|-------------|
| `conference.created` | Conference created |
| `conference.member.joined` | Member joined |
| `conference.member.left` | Member left |
| `conference.member.muted` | Member muted |
| `conference.member.unmuted` | Member unmuted |
| `conference.destroyed` | Conference destroyed |
| `conference.error` | Conference error |
| `conference.seat_replace.started` | Seat replacement transaction started |
| `conference.seat_replace.succeeded` | Seat replacement completed successfully |
| `conference.seat_replace.failed` | Seat replacement failed (rollback attempted) |
| `conference.seat_replace.rollback_failed` | Rollback failed after replacement failure |

### 6.9 Seat Replacement Event Ordering

The server emits explicit seat-replacement lifecycle events in addition to member join/left events.

Success path:

1. `conference_seat_replace_started`
2. `conference_member_left` (old seat)
3. `conference_member_joined` (new seat)
4. `conference_seat_replace_succeeded`

Failure path (with rollback):

1. `conference_seat_replace_started`
2. `conference_member_left` (old seat)
3. `conference_member_joined` (old seat rollback)
4. `conference_seat_replace_failed`

Failure path (rollback also fails):

1. `conference_seat_replace_started`
2. `conference_member_left` (old seat)
3. `conference_seat_replace_rollback_failed`
4. `conference_seat_replace_failed`

## 7. Error Handling

Command failures are reported via `command_failed` events:

```json
{
  "type": "command_failed",
  "action_id": "req-001",
  "action": "call.answer",
  "call_id": "c_92f4",
  "error": "Call not found: c_92f4"
}
```

Common error messages:

| Error Pattern | Description |
|---------------|-------------|
| `Call not found: <id>` | Call ID does not exist |
| `Command failed: <reason>` | Generic command execution failure |
| `Not implemented: <feature>` | Feature is not yet implemented |
| `invalid state` | Call state does not allow this operation |
| `already owned` | Call is owned by another session |

## 8. Event Types

### 8.1 Context Subscription

RWI supports multiple clients connecting simultaneously. Each connection is independently authenticated. A client can receive inbound call events by subscribing to contexts.

- **Context**: routing label that maps inbound calls to interested clients
- **Ownership**: each active call has exactly one controlling client at a time. Only the owner can issue control actions.
- **Fan-out**: `call.incoming` is delivered to all clients subscribed to the matching context. Ownership is determined by first-claim.

### 8.2 Call Dispatch Flow

```
1. SIP INVITE → RustPBX proxy
2. Dialplan routing resolves: app=rwi, context="ivr_bot"
3. RustPBX creates RwiApp for the call, holds it in ringing state
4. RwiGateway fans out call.incoming to ALL clients subscribed to "ivr_bot"
5. Client(s) receive call.incoming and may call.answer / call.reject to claim
6. First valid call.answer wins → that client becomes owner
7. If no client responds within no_answer_timeout_secs:
   → server executes no_answer_action (hangup, transfer, or play tone)
```

### 8.3 Outbound Call Ownership

Calls originated via `call.originate` are owned by the originating client immediately—no subscribe or attach step is needed.

## 9. Configuration

```toml
[rwi]
max_connections = 2000
max_calls_per_connection = 200
orphan_hold_secs = 30
originate_rate_limit = 10

# AMI tokens — no login action required
[[rwi.tokens]]
token = "secret-control-token"
scopes = ["call.control", "queue.control", "record.control"]

[[rwi.tokens]]
token = "secret-supervisor-token"
scopes = ["call.control", "supervisor.control", "media.stream"]

[[rwi.tokens]]
token = "secret-bot-token"
scopes = ["call.control", "media.stream"]

# Contexts define how inbound calls are dispatched to RWI clients
[[rwi.contexts]]
name = "ivr_bot"
no_answer_timeout_secs = 10
no_answer_action = "hangup"

[[rwi.contexts]]
name = "queue_agent_1"
no_answer_timeout_secs = 30
no_answer_action = "transfer"
no_answer_transfer_target = "sip:voicemail@local"
```

## 10. Security

1. **Authentication**:
   - Static AMI token passed as `Authorization: Bearer <ami_token>` HTTP header on WebSocket upgrade
   - Tokens configured statically in `[rwi.tokens]`
   - Token-less or invalid-token upgrades rejected with `HTTP 401 Unauthorized`

2. **Authorization**:
   - Per-token RBAC scopes (`call.control`, `queue.control`, `supervisor.control`, `media.stream`)
   - Per-call ownership checks on every operation

3. **Transport**:
   - Use `wss` in production
   - Optional mTLS support

## 11. Command Implementation Status

> Last updated: 2026-08-14

### Legend
- ✅ **Fully Implemented** - Command fully functional
- ⚠️ **Partially Implemented** - Command works but with limitations
- 🔧 **Stub/TODO** - Command accepted but actual functionality not complete

### Implementation Status by Category

| Category | Status | Notes |
|----------|--------|-------|
| **Session Commands** | ✅ Complete | All session commands fully implemented |
| **Call Control** | ✅ Complete | Originate, answer, hangup, bridge, transfer all working |
| **Media Playback** | ✅ Complete | Play, stop, hold music fully functional |
| **Recording** | ✅ Complete | Start, pause, resume, stop implemented; inline `record` on `call.originate` / `outbound/dial` (auto-start on answer); CDR carries the file → `recording_metadata_available` + `record_end` |
| **Queue** | ✅ Complete | Enqueue, dequeue, hold, unhold working |
| **Supervisor** | ⚠️ Partial | Commands implemented, **actual audio mixing TODO** |
| **Conference** | ⚠️ Partial | Create/add/remove/destroy working, **mute/unmute in mixer TODO** |
| **Media Stream/Inject** | ➖ Superseded | Commands removed — use `call.transfer` → `voip_bridge:` for bidirectional PCM |
| **SIP Messages** | ✅ Complete | `sip.message` / `sip.notify` / `sip.options_ping` construct and send real SIP requests |

### Known Limitations

1. **Track Muting**: track muting is exposed through the Console REST API (`mute` / `unmute` actions on `/api/calls/{id}/command`), not through the RWI WebSocket.

2. **Conference Muting**: `conference.mute` / `conference.unmute` emit events but do not actually mute audio in the mixer.

3. **PCM Stream**: `media.stream_start` / `media.inject_start` were removed. Real-time bidirectional PCM is provided by `call.transfer` to a `voip_bridge:` / `bridge:` WebSocket endpoint (works for inbound and outbound calls).

4. **SDP Renegotiation**: Hold/reinvite SDP renegotiation is TODO.

---

## 12. Smart Routing and Rule Engine (DESIGN PROPOSAL — NOT IMPLEMENTED)

> **Status**: the `[rwi.smart_routing]` / `[rwi.local_rules]` /
> `[rwi.sip_header_passthrough]` configuration described in this chapter is a
> design proposal. None of it is parsed or executed by the current code —
> these TOML sections are silently ignored. Do not rely on it in production.

RWI supports intelligent in-dialog message routing and local rule execution for high-reliability call center scenarios.

### 12.1 Three-Layer Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 3: RWI Application                                    │
│         Complex business logic, real-time AI decision        │
├─────────────────────────────────────────────────────────────┤
│ Layer 2: Local Rule Engine                                  │
│         Fallback rules when RWI disconnected                 │
│         Hotkey-triggered local actions                       │
├─────────────────────────────────────────────────────────────┤
│ Layer 1: Realtime Processing (SIP/RTP)                      │
│         DTMF auto-forward, INFO/OPTIONS passthrough          │
│         <10ms latency, always available                      │
└─────────────────────────────────────────────────────────────┘
```

### 12.2 Message Routing Configuration

```toml
[rwi.smart_routing]
enabled = true

# DTMF handling
[rwi.smart_routing.dtmf]
handling = "smart_forward"  # passthrough, local_rules, smart_forward, rwi_controlled
log_to_cdr = true

[[rwi.smart_routing.dtmf.hotkeys]]
sequence = "*9"
action = "forward_rwi"      # forward_leg, forward_rwi, execute_rule, auto_reply, drop

[[rwi.smart_routing.dtmf.hotkeys]]
sequence = "*0"
action = "execute_rule"
rule_id = "emergency_escalation"

# In-dialog INFO/OPTIONS/MESSAGE routing
[rwi.smart_routing.in_dialog]
enabled = true
notify_rwi = true           # Notify RWI even when forwarding

[[rwi.smart_routing.in_dialog.rules]]
name = "Route INFO to RWI"
priority = 100
enabled = true
method = "INFO"
content_type = "application/*"
action = { type = "forward_rwi", wait_response = true, timeout_ms = 5000 }

[[rwi.smart_routing.in_dialog.rules]]
name = "Auto-reply OPTIONS"
priority = 200
enabled = true
method = "OPTIONS"
action = { type = "auto_reply", code = 200 }
```

### 12.3 DTMF Handling Modes

| Mode | Behavior | Use Case |
|------|----------|----------|
| `passthrough` | Forward all DTMF to peer | Default, minimal latency |
| `local_rules` | Execute local rules only | Self-contained IVR |
| `smart_forward` | Passthrough + hotkey detection | Contact center with hotkeys |
| `rwi_controlled` | Buffer and forward to RWI | Complex multi-digit input |

### 12.4 Local Rule Engine

When RWI is disconnected or `action = "execute_rule"` is triggered, the Local Rule Engine executes predefined actions:

**Available Actions:**
- `originate` - Originate new call
- `bridge` - Bridge to another call
- `hangup` - Hangup with optional reason
- `play_prompt` - Play audio file
- `send_dtmf` - Send DTMF to peer
- `conference_add` - Add to conference
- `sequence` - Execute multiple actions in order
- `conditional` - Branch based on conditions

**Example Rule:**
```toml
[[rwi.local_rules]]
id = "emergency_escalation"
enabled = true

[[rwi.local_rules.actions]]
action = "play_prompt"
audio_file = "sounds/transferring.wav"

[[rwi.local_rules.actions]]
action = "originate"
destination = "sip:supervisor@backup-pbx.local"
caller_id = "Emergency Hotkey"
timeout_secs = 30
```

### 12.5 Graceful Degradation

When RWI connection is lost:
1. Active calls continue (Layer 1)
2. Fallback rules auto-execute for new events (Layer 2)
3. Calls can be recovered on RWI reconnection

```toml
[rwi.smart_routing.fallback]
when_rwi_disconnected = "execute_rules"  # passthrough, execute_rules, auto_hangup
rules = ["maintain_call", "log_cdr"]
```

## 13. Limitations and Notes

1. **SIP header passthrough**: `sip_headers` in `call.incoming` is read-only; the `[rwi.sip_header_passthrough]` whitelist described in the design chapter does not exist — the INVITE's headers are forwarded as captured.

2. **PCM stream**: Provided via `call.transfer` → `voip_bridge:` WebSocket endpoint (bidirectional PCM16 binary frames); the old `media.stream_start` / `media.inject_start` commands are removed.

3. **External MCU**: External conference backend requires SIP MCU server integration.

4. **Presence**: Agent presence state is not managed in RWI. Use a separate Presence service.

5. **Supervisor audio**: MediaMixer framework is in place but actual audio stream mixing is not yet connected.

6. **3PCC Originate**: TransferController 3PCC fallback integration with originate is TODO (marked in code).
