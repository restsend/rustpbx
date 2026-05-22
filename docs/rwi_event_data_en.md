# RWI Event Data Configuration

> RWI (Real-time WebSocket Interface) event types, data structures, and dispatch mechanisms.
> This document covers all RWI events defined in `src/rwi/proto.rs` and their configuration.

---

## 1. Overview

RWI provides real-time event streaming over WebSocket for call lifecycle, IVR flow, recording, queue/ACD, conference, agent state, and other telephony events. Events are serialized as JSON over WebSocket using a versioned envelope.

### Flat Call Context on Every Event

All call-scoped RWI events carry a flat `EventCallContext` struct that is `#[serde(flatten)]` into the JSON — no nested objects. Each event's type definition explicitly lists these fields:

```json
{
  "call_ringing": {
    "call_id": "call-abc123",
    "caller": "1001",
    "callee": "2000",
    "ani": "330909",
    "dnis": "9242000001",
    "direction": "inbound"
  }
}
```

The context is populated from `CallMetaStore` at gateway dispatch time: `RwiApp::on_enter()` stores caller/callee/direction when the call arrives, and every subsequent event emitted via `send_event_to_session` is enriched by `RwiEvent::enrich()` before serialization.

All fields are `Option<T>` with `#[serde(skip_serializing_if = "Option::is_none")]` — they only appear in JSON when populated. Consumers always see the exact fields defined in the Rust types:

### Envelope Format

```json
{
  "rwi": "1.0",
  "<event_type>": { ... }
}
```

### Connection

- **Endpoint**: `ws://<host>:<port>/rwi/ws`
- **Sub-protocol**: `rwi-v1`
- **Auth**: HTTP Header `Authorization: Bearer <token>` or URL param `?token=<token>`

### Subscription

```json
{
  "rwi": "1.0",
  "action_id": "sub-001",
  "action": "session.subscribe",
  "params": {
    "contexts": ["queue:support", "agent:*"]
  }
}
```

| Context Format | Description |
|----------------|-------------|
| `queue:<queue_id>` | Subscribe to queue events |
| `agent:<agent_id>` | Subscribe to agent events |
| `*` | Wildcard (all broadcast events) |

### Event Dispatch Methods

| Method | Recipient | Buffered | Description |
|--------|-----------|----------|-------------|
| `send_event_to_call_owner` | Session owning the call_id | ✅ Yes | Per-call fine-grained events |
| `broadcast_event` | All online sessions | ❌ No | Global agent/queue events |
| `fan_out_event_to_context` | Sessions subscribed to context | ❌ No | Incoming call notifications |

### Session Resume

Disconnected sessions can resume with buffered replay:

```json
{
  "rwi": "1.0",
  "action_id": "resume-001",
  "action": "session.resume",
  "params": { "last_sequence": 42 }
}
```

- Max buffer: 1000 events, retained 60 seconds
- Each event has a monotonically increasing `sequence` number
- Server replays all buffered events after `last_sequence`

---

## 2. Developer Quickstart

### Connecting and Receiving Events

#### Python (websockets)

```python
import asyncio, json
from websockets import connect

async def main():
    async with connect(
        "ws://pbx.example.com/rwi/ws",
        additional_headers={"Authorization": "Bearer your-token"},
        subprotocols=["rwi-v1"],
    ) as ws:
        # 1. Subscribe to all events
        await ws.send(json.dumps({
            "rwi": "1.0",
            "action_id": "sub-001",
            "action": "session.subscribe",
            "params": {"contexts": ["*"]}
        }))

        # 2. Receive events
        async for msg in ws:
            payload = json.loads(msg)
            for event_type, event_data in payload.items():
                if event_type == "rwi":
                    continue
                call_id = event_data.get("call_id", "")
                print(f"[{event_type}] call={call_id} data={event_data}")

asyncio.run(main())
```

#### JavaScript / Node.js

```javascript
const ws = new WebSocket("ws://pbx.example.com/rwi/ws", "rwi-v1", {
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

#### curl (test a single event via webhook)

```bash
# Configure webhook endpoint first (Settings → Proxy → RWI Webhook),
# then receive POST events at your endpoint:
curl -X POST https://myapp.example.com/rwi-hook \
  -H "Authorization: Bearer my-token" \
  -d '{"status": "ok"}'
```

### Attaching to a Call

To receive per-call events (not just broadcasts), attach your session to a specific call:

```json
{
  "rwi": "1.0",
  "action_id": "attach-001",
  "action": "session.attach_call",
  "params": {
    "call_id": "call-abc123",
    "mode": "listen"
  }
}
```

| Mode | Description |
|------|-------------|
| `control` | Full control: send DTMF, play audio, transfer, etc. |
| `listen` | Receive events only, no commands |
| `whisper` | Listen + whisper to agent (supervisor) |
| `barge` | Full barge-in (supervisor joins call) |

### Event Processing Flow

```
Client connects → subscribe → receive events
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
     broadcast       context       call_owner
     (all agents,   (queue,       (attached
      system-wide)   agent)        calls)
```

- **Broadcast events**: delivered to ALL connected sessions (no attach needed)
- **Context events**: delivered to sessions subscribed to matching context (`queue:support`, `agent:1001`)
- **Call owner events**: delivered only to the session that owns/attached the call

---

## 3. Complete Event Dictionary

> All call-scoped events include the following common flat-context fields (when available): `caller`, `callee`, `ani`, `dnis`, `direction`, `trunk`, `app_id`, `routing_target`, `agent_id`, `agent_name`. These are omitted from individual listings for brevity.

### 3.1 Call Lifecycle Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `call_incoming` | fan_out | New call enters the system. First event in any call flow. | `call_id`, `context`, `caller`, `callee`, `dial_direction`, `trunk`, `ani`, `dnis`, `called_phone`, `app_id`, `routing_target`, `uuid`, `routing_path`, `root_call_id`, `sip_headers` |
| `call_ringing` | owner | Outbound call is ringing (180 Ringing received) or inbound early media. | `call_id` |
| `call_early_media` | owner | 183 Session Progress with SDP received before answer. | `call_id` |
| `call_answered` | owner | Call answered (200 OK). | `call_id` |
| `call_bridged` | owner | Two call legs successfully bridged together. | `leg_a`, `leg_b` |
| `call_unbridged` | owner | Bridge between two legs released. | `call_id` |
| `call_hangup` | owner | Call ended. Final event in call lifecycle. | `call_id`, `reason?`, `sip_status?` |
| `call_no_answer` | owner | Outbound call timed out or remote no answer. | `call_id` |
| `call_busy` | owner | Remote party busy (486/600). | `call_id` |

### 3.2 Transfer Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `call_transferred` | owner | Transfer completed (blind/attended/3PCC). | `call_id` |
| `call_transfer_accepted` | owner | Remote accepted transfer request (REFER 202 / NOTIFY 200). | `call_id` |
| `call_transfer_failed` | owner | Transfer failed at any stage. | `call_id`, `sip_status?`, `reason?` |

### 3.3 Media Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `media_hold_started` | owner | Call placed on hold. | `call_id` |
| `media_hold_stopped` | owner | Call taken off hold. | `call_id` |
| `media_play_started` | ❌ | Audio playback started. | `call_id`, `leg_id?`, `track_id` |
| `media_play_finished` | fan_out | Audio playback finished (natural end or interrupted by DTMF). | `call_id`, `leg_id?`, `track_id`, `interrupted` |
| `media_stream_started` | owner | Media stream (send/recv) activated. | `call_id` |
| `media_stream_stopped` | owner | Media stream deactivated. | `call_id` |
| `media_ringback_passthrough_started` | owner | Ringback tone passthrough from source to target leg. | `source`, `target` |
| `media_ringback_passthrough_stopped` | ❌ | Ringback passthrough stopped. | `source`, `target` |
| `dtmf` | fan_out | Single DTMF digit received on a call leg. | `call_id`, `digit`, `leg_id?` |
| `dtmf_collected` | owner | All requested DTMF digits collected (max digits met or terminator pressed). | `call_id`, `leg_id`, `digits` |
| `dtmf_collection_timeout` | owner | DTMF collection timed out before min digits reached. | `call_id`, `leg_id` |

### 3.4 Recording Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `record_started` | owner | Recording started on a call. | `call_id`, `recording_id` |
| `record_paused` | owner | Recording paused. | `call_id`, `recording_id` |
| `record_resumed` | owner | Recording resumed after pause. | `call_id`, `recording_id` |
| `record_stopped` | owner | Recording stopped. Includes rich metadata when available. | `call_id`, `recording_id`, `duration_secs?`, `filename?`, `unique_id?`, `file_size?`, `download_url?`, `ani?`, `dnis?`, `called_phone?`, `call_type?`, `agent_id?`, `agent_name?`, `call_start_time?`, `call_end_time?`, `upload_time?`, `switch_flag?`, `root_call_id?` |
| `record_failed` | ❌ | Recording failed. | `call_id`, `recording_id`, `error` |
| `recording_metadata_available` | ❌ | Full recording metadata available (separate from record_stopped). | `call_id`, `recording_id`, `metadata` (full struct) |

### 3.5 IVR Events

All IVR events carry the flat `EventCallContext` fields (`caller`, `callee`, `ani`, `dnis`, `direction`) when available.

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `ivr_node_entered` | fan_out | ✅ Call entered an IVR node/menu. Tree mode emits on `enter_menu()`. | `call_id`, `node_id`, `node_name`, `node_type`, `app_id`, `entry_time`, `ani?`, `dnis?`, `routing_target?`, `previous_node_id?` |
| `ivr_node_exited` | fan_out | ✅ Call exited an IVR node (DTMF choice, timeout, etc.). Emitted before executing the chosen action. | `call_id`, `node_id`, `node_name`, `result_value?`, `duration_ms`, `exit_time`, `next_node_id?`, `hangup_reason?`, `call_result?` |
| `ivr_flow_transitioned` | ❌ | Call transitioned between IVR applications. | `call_id`, `from_app_id`, `to_app_id`, `from_node_id`, `to_node_id`, `transition_reason`, `transition_time`, `next_routing_target?` |
| `ivr_flow_completed` | ✅ | IVR flow finished (terminal action: Transfer, Queue, Voicemail, Hangup). Emitted in `tree_app.rs` `execute_action()`. | `call_id`, `app_id`, `total_nodes_traversed`, `total_duration_ms`, `final_result`, `completion_time`, `final_routing_target?` |
| `ivr_step_trace` | fan_out | Step-mode IVR debug trace entry (one per provider round-trip). | `call_id`, `session_id`, `caller`, `callee`, `timestamp`, `step_index`, `event_type`, `action_type`, `action_json?`, `result_kind`, `duration_ms`, `error?` |

### 3.6 Queue / ACD Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `queue_joined` | owner/broadcast | Call entered a queue. | `call_id`, `queue_id` |
| `queue_position_changed` | ❌ | Call's position in queue changed. | `call_id`, `queue_id`, `position` |
| `queue_agent_offered` | broadcast | Agent manually assigned to a queued call. | `call_id`, `queue_id`, `agent_id` |
| `queue_agent_connected` | ❌ | ACD successfully connected an agent to a queued call. | `call_id`, `queue_id`, `agent_id` |
| `queue_left` | broadcast | Call left the queue (dequeue/requeue/answered). | `call_id`, `queue_id`, `reason?` |
| `queue_wait_timeout` | ❌ | Call exceeded max wait time in queue. | `call_id`, `queue_id` |
| `queue_overflowed` | owner | Queue full — call overflowed to another queue. | `call_id`, `original_queue_id`, `overflow_queue_id`, `reason` |
| `queue_voicemail_redirected` | owner | Queue full — call redirected to voicemail. | `call_id`, `queue_id`, `reason` |
| `queue_candidates_found` | ❌ | ACD found candidate agents for a queued call. | `call_id`, `queue_id`, `candidates[]`, `trace_id` |
| `queue_agent_ringing` | ❌ | ACD ringing a candidate agent. | `call_id`, `queue_id`, `agent_id`, `trace_id` |
| `queue_agent_no_answer` | ❌ | Candidate agent did not answer. | `call_id`, `queue_id`, `agent_id`, `attempt`, `trace_id` |
| `queue_agent_rejected` | ❌ | Candidate agent rejected the call. | `call_id`, `queue_id`, `agent_id`, `attempt`, `trace_id` |
| `queue_fallback_executed` | ❌ | ACD fallback strategy executed (e.g., overflow, voicemail). | `call_id`, `queue_id`, `action`, `reason`, `trace_id` |
| `queue_alert` | broadcast | SLA threshold breached for a queue. | `queue_id`, `alert_type`, `message` |

### 3.7 Agent Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `agent_state_changed` | broadcast | ✅ Agent state machine transitioned. Emitted from `AgentRegistry::update_status()` and `try_reserve_agent()` via event_tx → RwiGateway. | `agent_id`, `from_status`, `to_status`, `call_id?`, `agent_name?`, `agent_extension?`, `dn?`, `team_id?`, `duration_secs?`, `reason_code?` |

### 3.8 DN Events (Directory Number)

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `dn_state_changed` | broadcast | Extension/DN signaling event (ringing, established, held, etc.). | `dn`, `event_code`, `event_name`, `system_time`, `call_id?`, `kz_conn_id?`, `agent_id?`, `other_dn?`, `ani?`, `dnis?`, `reason_code?`, `agent_work_mode?`, `releasing_party?`, `third_party_dn?`, `vq_name?`, `routing_target?`, `skill_group?`, `target_dn?` |
| `dn_registered` | broadcast | Extension registered (softphone login). | `dn`, `agent_id?`, `register_time` |
| `dn_unregistered` | broadcast | Extension unregistered (softphone logout). | `dn`, `agent_id?`, `unregister_time` |

### 3.9 Call Metadata Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `call_metadata_updated` | ❌ | Call metadata updated after initial `call_incoming`. | `call_id`, `metadata` (full `CallMetadata` struct) |

### 3.10 Conference Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `conference_created` | broadcast | Conference room created. | `conf_id` |
| `conference_member_joined` | broadcast | Member joined the conference. | `conf_id`, `call_id` |
| `conference_member_left` | broadcast | Member left the conference. | `conf_id`, `call_id` |
| `conference_member_muted` | broadcast | Member muted. | `conf_id`, `call_id` |
| `conference_member_unmuted` | broadcast | Member unmuted. | `conf_id`, `call_id` |
| `conference_destroyed` | broadcast | Conference room destroyed. | `conf_id` |
| `conference_error` | ❌ | Conference operation error. | `conf_id`, `error` |
| `conference_consult_dialing` | ❌ | Consult call is being dialed. | `call_id`, `target` |
| `conference_consult_connected` | ❌ | Consult call connected. | `call_id`, `target` |
| `conference_merge_requested` | fan_out | Merge of consultation call requested. | `call_id`, `consultation_call_id` |
| `conference_merged` | fan_out | Conference merge succeeded. | `conf_id`, `call_id` |
| `conference_merge_failed` | fan_out | Conference merge failed. | `conf_id`, `call_id`, `reason` |
| `conference_seat_replace_started` | fan_out | Seat replacement started (e.g., attended transfer). | `conf_id`, `old_call_id`, `new_call_id` |
| `conference_seat_replace_succeeded` | fan_out | Seat replacement complete. | `conf_id`, `old_call_id`, `new_call_id` |
| `conference_seat_replace_failed` | fan_out | Seat replacement failed. | `conf_id`, `old_call_id`, `new_call_id`, `reason` |
| `conference_seat_replace_rollback_failed` | fan_out | Seat replacement rollback (e.g., transfer cancelled) also failed. | `conf_id`, `old_call_id`, `new_call_id`, `reason` |

### 3.11 Supervisor Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `supervisor_listen_started` | ❌ | Supervisor started listening to agent call. | `supervisor_call_id`, `target_call_id` |
| `supervisor_whisper_started` | ❌ | Supervisor started whispering to agent. | `supervisor_call_id`, `target_call_id` |
| `supervisor_barge_started` | ❌ | Supervisor barged into agent call. | `supervisor_call_id`, `target_call_id` |
| `supervisor_takeover_started` | ❌ | Supervisor took over call from agent. | `supervisor_call_id`, `target_call_id` |
| `supervisor_mode_stopped` | owner | Supervisor mode ended (listen/whisper/barge/takeover). | `supervisor_call_id`, `target_call_id` |

### 3.12 Parallel Originate Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `parallel_originate_started` | owner | Parallel originate task started with multiple legs. | `operation_id`, `leg_count` |
| `parallel_originate_leg_ringing` | ❌ | One leg of parallel originate is ringing. | `operation_id`, `call_id`, `destination` |
| `parallel_originate_winner` | owner | First leg to answer (wins the parallel originate). | `operation_id`, `call_id`, `destination` |
| `parallel_originate_leg_cancelled` | owner | Remaining legs cancelled after winner found, or a leg failed. | `operation_id`, `call_id`, `reason` |
| `parallel_originate_completed` | owner | Parallel originate task completed. | `operation_id`, `winning_call_id` |
| `parallel_originate_failed` | owner | All legs failed or timed out. | `operation_id`, `reason` |

### 3.13 SIP Signaling Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `sip_message_received` | runtime | SIP MESSAGE (IM) received on a call leg. | `call_id`, `content_type`, `body` |
| `sip_notify_received` | runtime | SIP NOTIFY received on a call. | `call_id`, `event`, `content_type`, `body` |

### 3.14 Session System Events

| Event | Dispatch | Description | Key Fields |
|-------|----------|-------------|------------|
| `session_resumed` | ❌ | WebSocket session resumed after disconnect. | `session_id`, `last_sequence` |
| `call_ownership_changed` | ❌ | Call ownership transferred between sessions. | `call_id`, `session_id`, `mode` |

### Implementation Status Legend

| Status | Meaning | Count |
|--------|---------|-------|
| (no mark) | Implemented with emission point | 36 |
| ❌ | Defined in RwiEvent proto but NO emission point implemented | 21 |

---

## 4. Call Events (Detailed)

All call-scoped events except `call_incoming` are **automatically enriched** with contextual fields from `CallMetaStore` (populated when the call first enters). The fields `caller`, `callee`, `ani`, `dnis`, `direction`, `trunk`, `app_id`, `routing_target`, `agent_id`, and `agent_name` are injected at the JSON serialization layer — **no changes to event producers** are needed.

| Event | Dispatch | Key Fields (incl. auto-enriched) | Trigger |
|-------|----------|---------------------------------------|---------|
| `call_incoming` | fan_out_to_context | `call_id`, `context`, `caller`, `callee`, `direction`, `trunk`, `sip_headers`, `ani`, `dnis`, `called_phone`, `app_id`, `routing_target`, `uuid`, `routing_path`, `root_call_id` | Incoming call enters RWI App |
| `call_ringing` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Outbound ringing or inbound 180 |
| `call_early_media` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | 183 early media received |
| `call_answered` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Call answered (200 OK) |
| `call_bridged` | call_owner (both legs) | `leg_a`, `leg_b` | Two legs bridged |
| `call_unbridged` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Bridge released |
| `call_hangup` | call_owner | `call_id`, `reason?`, `sip_status?`, `caller`, `callee`, `ani`, `dnis`, `direction` | Call hangup |
| `call_no_answer` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Outbound timeout/no answer |
| `call_busy` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Remote busy (486/600) |

### CallIncomingData Structure

```json
{
  "call_id": "call-abc123",
  "context": "inbound",
  "caller": "sip:1001@example.com",
  "callee": "sip:queue@example.com",
  "dial_direction": "inbound",
  "trunk": "trunk-sip",
  "sip_headers": { "X-Custom": "value" },
  "root_call_id": "call-root-42",
  "ani": "330909",
  "dnis": "9242000001",
  "called_phone": "018659727661",
  "app_id": "ivr-support-main",
  "routing_target": "queue:support",
  "uuid": "0200M6NJ54CGH3AH1K8482LAES4OTFEL",
  "routing_path": ["menu:root", "queue:level1"]
}
```

---

## 5. Call Metadata Events

### CallMetadataUpdated

Emitted when call metadata is updated after the initial `call_incoming`.

```json
{
  "call_metadata_updated": {
    "call_id": "call-abc",
    "metadata": {
      "root_call_id": "call-root-42",
      "ani": "330909",
      "dnis": "9242000001",
      "called_phone": "018659727661",
      "dial_direction": "inbound",
      "uuid": "uuid-abc-123",
      "routing_path": ["menu:root", "queue:level1"],
      "app_id": "ivr-support-main",
      "routing_target": "queue:support",
      "switch_name": "SIP_Switch_KS"
    }
  }
}
```

### CallMetadata Fields

| Field | Type | Description |
|-------|------|-------------|
| `root_call_id` | Option\<String\> | Root call identifier (constant across transfers) |
| `ani` | Option\<String\> | Calling party number |
| `dnis` | Option\<String\> | Dialed number |
| `called_phone` | Option\<String\> | Actual dialed phone number |
| `dial_direction` | Option\<String\> | Call direction (inbound/outbound/internal) |
| `uuid` | Option\<String\> | Global UUID for recording linkage |
| `routing_path` | Option\<Vec\<String\>\> | Routing path sequence |
| `app_id` | Option\<String\> | IVR application identifier |
| `routing_target` | Option\<String\> | Current routing target |
| `switch_name` | Option\<String\> | SBC/switch node name |

---

## 6. Transfer Events

All transfer events are auto-enriched with `caller`, `callee`, `ani`, `dnis`, `direction`.

| Event | Dispatch | Key Fields (incl. auto-enriched) | Trigger |
|-------|----------|-----------------------------------|---------|
| `call_transferred` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Transfer completed (blind/attended/3PCC) |
| `call_transfer_accepted` | call_owner | `call_id`, `caller`, `callee`, `ani`, `dnis`, `direction` | Remote accepted transfer (REFER 202/NOTIFY 200) |
| `call_transfer_failed` | call_owner | `call_id`, `sip_status?`, `reason?`, `caller`, `callee`, `ani`, `dnis`, `direction` | Transfer failed |

---

## 7. Media Events

All media events are auto-enriched with `caller`, `callee`, `ani`, `dnis`, `direction`.

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `media_hold_started` | ✅ | call_owner | `call_id` | Call hold started |
| `media_hold_stopped` | ✅ | call_owner | `call_id` | Call hold ended |
| `media_play_started` | ⚠️ | — | `call_id`, `track_id` | Audio playback started |
| `media_play_finished` | ✅ | fan_out_to_context | `call_id`, `track_id`, `interrupted` | Audio playback finished (natural or interrupted by DTMF) |
| `media_stream_started` | ✅ | call_owner | `call_id` | Media stream started |
| `media_stream_stopped` | ✅ | call_owner | `call_id` | Media stream stopped |
| `media_ringback_passthrough_started` | ✅ | call_owner | `source`, `target` | Ringback passthrough started |
| `media_ringback_passthrough_stopped` | ⚠️ | — | `source`, `target` | Ringback passthrough stopped |
| `dtmf` | ✅ | fan_out_to_context | `call_id`, `digit`, `leg_id?` | DTMF digit received |

---

## 8. Recording Events

> **Note**: All recording events are auto-enriched with `caller`, `callee`, `ani`, `dnis`, `direction` from `CallMetaStore`. The `record_stopped` event also already includes its own embedded metadata fields (ani, dnis, called_phone, call_type, agent_id, agent_name, etc.).

### RecordStopped (Enhanced with Metadata)

```json
{
  "record_stopped": {
    "call_id": "call-abc",
    "recording_id": "rec-xyz",
    "duration_secs": 51,
    "filename": "0200M6NJ54CGH3AH1K8482LAES4OTFEL_2026-05-14_08-11-49.mp3",
    "unique_id": "0200M6NJ54CGH3AH1K8482LAES4OTFEL",
    "file_size": 149517,
    "download_url": "https://storage.example.com/recording.mp3",
    "ani": "330909",
    "dnis": "9242000001",
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

### RecordingMetadataAvailable (Separate Event Channel)

```json
{
  "recording_metadata_available": {
    "call_id": "call-abc",
    "recording_id": "rec-xyz",
    "metadata": {
      "filename": "rec_20260514.mp3",
      "unique_id": "uuid-123",
      "file_size": 149517,
      "download_url": "https://storage.example.com/rec.mp3",
      "ani": "330909",
      "dnis": "9242000001",
      "called_phone": null,
      "call_type": "inbound",
      "agent_id": "451447",
      "agent_name": "luoxiaofeng90_v",
      "call_start_time": "2026-05-14T08:11:35Z",
      "call_end_time": "2026-05-14T08:12:26Z",
      "upload_time": null,
      "switch_flag": "ks",
      "process_flag": "ks_22_normal",
      "kz_conn_id": null,
      "root_call_id": null
    }
  }
}
```

### RecordingMetadata Fields

| Field | Type | Description |
|-------|------|-------------|
| `filename` | String | Recording filename |
| `unique_id` | String | Recording UUID |
| `file_size` | u64 | File size in bytes |
| `download_url` | Option\<String\> | Download URL |
| `ani` | Option\<String\> | Calling party number |
| `dnis` | Option\<String\> | Dialed number |
| `called_phone` | Option\<String\> | Actual called number |
| `call_type` | String | inbound/outbound/internal/consult |
| `agent_id` | Option\<String\> | Agent ID (uid) |
| `agent_name` | Option\<String\> | Agent username |
| `call_start_time` | Option\<String\> | Call start timestamp |
| `call_end_time` | Option\<String\> | Call end timestamp |
| `upload_time` | Option\<String\> | Upload completion timestamp |
| `switch_flag` | Option\<String\> | Site identifier (ks/bj/hz) |
| `process_flag` | Option\<String\> | Process identifier (e.g., "ks_22_normal") |
| `kz_conn_id` | Option\<String\> | Call connection ID |
| `root_call_id` | Option\<String\> | Root call ID for linkage |

### Recording Event Summary

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `record_started` | ✅ | call_owner | `call_id`, `recording_id` | Recording started |
| `record_paused` | ✅ | call_owner | `call_id`, `recording_id` | Recording paused |
| `record_resumed` | ✅ | call_owner | `call_id`, `recording_id` | Recording resumed |
| `record_stopped` | ✅ | call_owner | `call_id`, `recording_id`, `duration_secs?`, +enhanced fields | Recording stopped |
| `record_failed` | ⚠️ | — | `call_id`, `recording_id`, `error` | Recording failed |
| `recording_metadata_available` | ✅ | call_owner | `call_id`, `recording_id`, `metadata` | Recording metadata ready |

---

## 9. IVR Events

> **Note**: IVR events are auto-enriched with `caller`, `callee`, `direction` from `CallMetaStore`. The `ivr_node_entered` event also includes `ani`, `dnis`, `routing_target` from the IVR engine itself.

### Event Types

#### IvrNodeEntered

Emitted when a call enters an IVR node.

```json
{
  "ivr_node_entered": {
    "call_id": "call-abc",
    "node_id": "node-001",
    "node_name": "main_menu.wav",
    "node_type": "menu",
    "app_id": "ivr-support-main",
    "entry_time": "2026-05-14T17:54:45.537Z",
    "ani": "17503062824",
    "dnis": "4000111666",
    "routing_target": "menu:root",
    "previous_node_id": null
  }
}
```

#### IvrNodeExited

Emitted when a call exits an IVR node.

```json
{
  "ivr_node_exited": {
    "call_id": "call-abc",
    "node_id": "node-001",
    "node_name": "main_menu.wav",
    "result_value": "1",
    "duration_ms": 4500,
    "exit_time": "2026-05-14T17:54:50.037Z",
    "next_node_id": "node-002",
    "hangup_reason": null,
    "call_result": null
  }
}
```

#### IvrFlowTransitioned

Emitted when a call transitions between IVR applications.

```json
{
  "ivr_flow_transitioned": {
    "call_id": "call-abc",
    "from_app_id": "ivr-support-main",
    "to_app_id": "ivr-billing",
    "from_node_id": "node-menu-1",
    "to_node_id": "node-menu-2",
    "transition_reason": "menu_choice",
    "transition_time": "2026-05-14T17:55:00.000Z",
    "next_routing_target": "menu:billing"
  }
}
```

#### IvrFlowCompleted

Emitted when an IVR flow completes.

```json
{
  "ivr_flow_completed": {
    "call_id": "call-abc",
    "app_id": "ivr-support-main",
    "total_nodes_traversed": 3,
    "total_duration_ms": 15200,
    "final_result": "transferred",
    "completion_time": "2026-05-14T17:55:00.000Z",
    "final_routing_target": "queue:support"
  }
}
```

#### IvrStepTrace (Step-Mode Debug Trace)

Real-time step trace for step-mode IVR debugging.

```json
{
  "ivr_step_trace": {
    "call_id": "call-abc",
    "session_id": "call_abc123",
    "caller": "1001",
    "callee": "2000",
    "timestamp": "2026-05-21T16:30:01.023Z",
    "step_index": 1,
    "event_type": "provider_response",
    "action_type": "Transfer",
    "action_json": "{\"type\":\"transfer\",\"target\":\"2001\"}",
    "result_kind": "terminal",
    "duration_ms": 42,
    "error": null
  }
}
```

### IVR Event Summary

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `ivr_node_entered` | ✅ | fan_out_to_context | `call_id`, `node_id`, `node_name`, `node_type`, `app_id`, `entry_time`, `ani?`, `dnis?`, `routing_target?`, `previous_node_id?` | Call enters an IVR node |
| `ivr_node_exited` | ✅ | fan_out_to_context | `call_id`, `node_id`, `node_name`, `result_value?`, `duration_ms`, `exit_time`, `next_node_id?`, `hangup_reason?`, `call_result?` | Call exits an IVR node |
| `ivr_flow_transitioned` | ✅ | fan_out_to_context | `call_id`, `from_app_id`, `to_app_id`, `from_node_id`, `to_node_id`, `transition_reason`, `transition_time`, `next_routing_target?` | Call transitions between IVR apps |
| `ivr_flow_completed` | ✅ | fan_out_to_context | `call_id`, `app_id`, `total_nodes_traversed`, `total_duration_ms`, `final_result`, `completion_time`, `final_routing_target?` | IVR flow completed |
| `ivr_step_trace` | ✅ | fan_out_to_context | `call_id`, `session_id`, `caller`, `callee`, `timestamp`, `step_index`, `event_type`, `action_type`, `action_json?`, `result_kind`, `duration_ms`, `error?` | Step-mode IVR debug trace |

### IvrNodeInfo Structure

```rust
pub struct IvrNodeInfo {
    pub node_id: String,
    pub node_name: String,
    pub node_type: String,
    pub routing_target: Option<String>,
    pub previous_node_id: Option<String>,
    pub next_node_id: Option<String>,
    pub duration_ms: Option<u32>,
    pub result_value: Option<String>,
}
```

### IvrFlowContext Structure

```rust
pub struct IvrFlowContext {
    pub app_id: String,
    pub routing_path: Vec<String>,
    pub service_type: Option<String>,
    pub customer_type: Option<String>,
}
```

---

## 10. Queue / ACD Events

> **Note**: All queue events are auto-enriched with `caller`, `callee`, `ani`, `dnis`, `direction` from `CallMetaStore`.

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `queue_joined` | ✅ | call_owner / broadcast | `call_id`, `queue_id` | Call enters queue |
| `queue_position_changed` | ❌ | — | `call_id`, `queue_id`, `position` | Queue position changed |
| `queue_agent_offered` | ✅ | broadcast | `call_id`, `queue_id`, `agent_id` | Agent manually assigned |
| `queue_agent_connected` | ❌ | — | `call_id`, `queue_id`, `agent_id` | ACD connected (unimplemented) |
| `queue_left` | ✅ | broadcast | `call_id`, `queue_id`, `reason?` | Left queue (dequeue/requeue) |
| `queue_overflowed` | ✅ | call_owner | `call_id`, `original_queue_id`, `overflow_queue_id`, `reason` | Queue overflow |
| `queue_voicemail_redirected` | ✅ | call_owner | `call_id`, `queue_id`, `reason` | Queue full → voicemail |
| `queue_candidates_found` | ❌ | — | `call_id`, `queue_id`, `candidates[]`, `trace_id` | ACD candidates found |
| `queue_agent_ringing` | ❌ | — | `call_id`, `queue_id`, `agent_id`, `trace_id` | ACD ringing agent |
| `queue_agent_no_answer` | ❌ | — | `call_id`, `queue_id`, `agent_id`, `attempt`, `trace_id` | Agent no answer |
| `queue_agent_rejected` | ❌ | — | `call_id`, `queue_id`, `agent_id`, `attempt`, `trace_id` | Agent rejected |
| `queue_wait_timeout` | ❌ | — | `call_id`, `queue_id` | Queue wait timeout |
| `queue_fallback_executed` | ❌ | — | `call_id`, `queue_id`, `action`, `reason`, `trace_id` | Fallback executed |
| `queue_alert` | ✅ | broadcast | `queue_id`, `alert_type`, `message` | SLA alert triggered |

> ❌ = Defined in proto but no emission point implemented. ACD engine integration is pending.

---

## 11. Agent State Events

| Event | Dispatch | Key Fields | Trigger |
|-------|----------|------------|---------|
| `agent_state_changed` | broadcast | `agent_id`, `from_status`, `to_status`, `call_id?`, `agent_name?`, `agent_extension?`, `dn?`, `team_id?`, `duration_secs?`, `reason_code?` | Agent state transition |

### Agent States

| State | Description | Can Transition To |
|-------|-------------|-------------------|
| `offline` | Disconnected | idle, away, dnd |
| `idle` | Ready to accept calls | ringing, away, dnd, custom, offline |
| `away` | Online but not accepting (break) | idle, dnd, custom, offline |
| `dnd` | Do not disturb (meeting/training) | idle, away, custom, offline |
| `ringing` | Ringing (call_id present) | busy (answer), idle (no answer) |
| `busy` | On a call (call_id present) | wrapup |
| `wrapup` | After-call work | idle, away, dnd, custom |
| `custom:<name>` | Custom state (training, lunch) | idle, away, dnd, offline |

---

## 12. DN Events (Directory Number / Extension)

Emitted for granular extension-level signaling events.

```json
{
  "dn_state_changed": {
    "dn": "80001",
    "event_code": 64,
    "event_name": "ESTABLISHED",
    "system_time": "2026-05-14T17:54:49.003Z",
    "call_id": "call-abc",
    "kz_conn_id": "kc-12345",
    "agent_id": "10001",
    "ani": "19534519769",
    "dnis": "39989"
  }
}
```

### DN Event Codes

| Code | Event | Object | Description | Key Fields |
|------|-------|--------|-------------|------------|
| 53 | REGISTERED | DN | Softphone login | `dn`, `agent_id` |
| 60 | RINGING | DN | Agent ringing | `dn`, `ani`, `other_dn`, `dnis` |
| 61 | DIALING | DN | Agent dialing | `dn`, `other_dn`, `call_id` |
| 64 | ESTABLISHED | DN | Call established | `dn`, `ani`, `dnis`, `call_id` |
| 65 | RELEASED | DN | Call released | `dn`, `call_id`, `releasing_party` |
| 59 | ABANDONED | DN/Queue | Abandoned during ringing | `dn`, `call_id`, `releasing_party` |
| 66 | HELD | DN | Agent hold | `dn`, `call_id` |
| 67 | RETRIEVED | DN | Hold retrieved | `dn`, `call_id` |
| 68 | PARTYCHANGED | DN | Multi-party changed | `dn`, `call_id`, `third_party_dn` |
| 69 | PARTYADDED | DN | Party added | `dn`, `call_id`, `third_party_dn` |
| 70 | PARTYDELETED | DN | Party removed | `dn`, `call_id`, `third_party_dn` |
| 73 | AGENTLOGIN | DN | Agent login | `agent_id`, `dn` |
| 74 | AGENTLOGOUT | DN | Agent logout | `agent_id`, `dn` |
| 75 | AGENTREADY | DN | Agent ready | `agent_id`, `dn` |
| 76 | AGENTNOTREADY | DN | Agent not ready | `agent_id`, `dn`, `reason_code` |
| 87 | ONHOOK | DN | On hook | `dn`, `agent_id` |
| 57 | QUEUED | Queue | Entered VQ | `dn`(VQ), `call_id`, `skill_group` |
| 58 | DIVERTED | Queue/Route | Assigned | `dn`, `target_dn`, `routing_target` |
| 102 | REMOTECONNECTIONFAILED | Route | Route failed | `dn`(route), `target_dn` |

### DN Event Summary

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `dn_state_changed` | ✅ | broadcast | All DN event fields | DN signaling event |
| `dn_registered` | ✅ | broadcast | `dn`, `agent_id?`, `register_time` | DN registered |
| `dn_unregistered` | ✅ | broadcast | `dn`, `agent_id?`, `unregister_time` | DN unregistered |

---

## 13. Conference Events

> **Note**: Conference member events (`member_joined`, `member_left`, `member_muted`, `member_unmuted`) and merge events are auto-enriched with `caller`, `callee`, `ani`, `dnis`, `direction` for the member call.

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `conference_created` | ✅ | broadcast | `conf_id` | Conference created |
| `conference_member_joined` | ✅ | broadcast | `conf_id`, `call_id` | Member joined |
| `conference_member_left` | ✅ | broadcast | `conf_id`, `call_id` | Member left |
| `conference_member_muted` | ✅ | broadcast | `conf_id`, `call_id` | Member muted |
| `conference_member_unmuted` | ✅ | broadcast | `conf_id`, `call_id` | Member unmuted |
| `conference_destroyed` | ✅ | broadcast | `conf_id` | Conference destroyed |
| `conference_merge_requested` | ✅ | fan_out | `call_id`, `consultation_call_id` | Merge requested |
| `conference_merged` | ✅ | fan_out | `conf_id`, `call_id` | Merge succeeded |
| `conference_merge_failed` | ✅ | fan_out | `conf_id`, `call_id`, `reason` | Merge failed |
| `conference_seat_replace_started` | ✅ | fan_out | `conf_id`, `old_call_id`, `new_call_id` | Seat replace started |
| `conference_seat_replace_succeeded` | ✅ | fan_out | `conf_id`, `old_call_id`, `new_call_id` | Seat replace succeeded |
| `conference_seat_replace_failed` | ✅ | fan_out | `conf_id`, `old_call_id`, `new_call_id`, `reason` | Seat replace failed |
| `conference_seat_replace_rollback_failed` | ✅ | fan_out | `conf_id`, `old_call_id`, `new_call_id`, `reason` | Rollback failed |
| `conference_error` | ⚠️ | — | `conf_id`, `error` | Conference error |
| `conference_consult_dialing` | ⚠️ | — | `call_id`, `target` | Consult call dialing |
| `conference_consult_connected` | ⚠️ | — | `call_id`, `target` | Consult call connected |

---

## 14. Supervisor Events

> **Note**: Supervisor events are auto-enriched with `caller`, `callee`, `ani`, `dnis`, `direction` for both `supervisor_call_id` and `target_call_id` when available.

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `supervisor_listen_started` | ❌ | — | `supervisor_call_id`, `target_call_id` | Listen started |
| `supervisor_whisper_started` | ❌ | — | `supervisor_call_id`, `target_call_id` | Whisper started |
| `supervisor_barge_started` | ❌ | — | `supervisor_call_id`, `target_call_id` | Barge started |
| `supervisor_takeover_started` | ❌ | — | `supervisor_call_id`, `target_call_id` | Takeover started |
| `supervisor_mode_stopped` | ✅ | call_owner | `supervisor_call_id`, `target_call_id` | Supervisor mode stopped |

---

## 15. Parallel Originate Events

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `parallel_originate_started` | ✅ | call_owner | `operation_id`, `leg_count` | Task started |
| `parallel_originate_leg_ringing` | ❌ | — | `operation_id`, `call_id`, `destination` | Leg ringing |
| `parallel_originate_winner` | ✅ | call_owner | `operation_id`, `call_id`, `destination` | First answered leg |
| `parallel_originate_leg_cancelled` | ✅ | call_owner | `operation_id`, `call_id`, `reason` | Other legs cancelled |
| `parallel_originate_completed` | ✅ | call_owner | `operation_id`, `winning_call_id` | Task completed |
| `parallel_originate_failed` | ✅ | call_owner | `operation_id`, `reason` | All legs failed/timed out |

---

## 16. SIP Signaling Events

| Event | Dispatch | Key Fields | Trigger |
|-------|----------|------------|---------|
| `sip_message_received` | Session runtime | `call_id`, `content_type`, `body` | SIP MESSAGE received |
| `sip_notify_received` | Session runtime | `call_id`, `event`, `content_type`, `body` | SIP NOTIFY received |

---

## 17. Session System Events

| Event | Status | Dispatch | Key Fields | Trigger |
|-------|--------|----------|------------|---------|
| `session_resumed` | ❌ | — | `session_id`, `last_sequence` | Session resume completed |
| `call_ownership_changed` | ❌ | — | `call_id`, `session_id`, `mode` | Call ownership changed |

---

## 18. Event Retrieval Matrix

| Scenario | Primary Channel | Fallback |
|----------|----------------|----------|
| Agent real-time state | RWI `agent_state_changed` (broadcast) | GET `/console/cc/agents/{id}` |
| Call distribution | ❌ ACD event chain incomplete | SIP MESSAGE |
| Call lifecycle (inbound) | RWI fan_out_to_context + call_owner | None |
| Call lifecycle (outbound) | RWI call_owner (attach first) | None |
| Queue assignment flow | ❌ `queue_*` ACD events missing | GET `/console/cc/queues/{id}/stats` |
| SLA alerts | RWI `queue_alert` (broadcast) | Prometheus `cc_sla_breached` |
| Recording status | RWI `record_*` (call_owner) | None |
| Supervisor listen/whisper/barge | ❌ `supervisor_*_started` missing | Only `supervisor_mode_stopped` |
| Conference member changes | RWI `conference_*` (broadcast) | None |
| Metrics | Prometheus `/metrics` | None |

---

## 19. Implementation Status Summary

| Status | Meaning |
|--------|---------|
| ✅ | Implemented with emission point |
| ⚠️ | Defined in RwiEvent proto but no emission point |
| ❌ | Not implemented |

### Priority for Completion

1. **Queue/ACD events** (highest priority): ACD engine integration for `candidates_found`, `agent_ringing`, `agent_no_answer`, `agent_rejected`, `connected`, `wait_timeout`, `fallback_executed`
2. **Supervisor events**: Mixer creation + event emission for `listen`, `whisper`, `barge`, `takeover` started events
3. **Missing emission points**: `record_failed`, `media_play_started`, `media_ringback_passthrough_stopped`, `call_ownership_changed`, `session_resumed`, conference consult events

---

## 20. RWI Webhook (HTTP Forwarding)

RWI events can be forwarded to external HTTP endpoints via the webhook mechanism. This allows third-party systems to consume RWI events without maintaining a persistent WebSocket connection.

### Architecture

```
RwiGateway (internal)
    │
    ├── fan_out_to_context() ──► WebSocket sessions (real-time)
    │
    └── forward_to_webhook() ──► broadcast::channel (512 buffer)
                                      │
                              ┌───────▼────────┐
                              │  Webhook Task  │  (background tokio task)
                              │  - Dedup cache │
                              │  - Event filter│
                              │  - HTTP POST   │
                              └───────┬────────┘
                                      │
                              ┌───────▼────────┐
                              │  External      │
                              │  HTTP Endpoint │
                              └────────────────┘
```

### Configuration

Configured in `rustpbx.toml` at the root level:

```toml
[rwi_webhook]
url = "https://hook.example.com/rwi-events"
timeout_ms = 5000
headers = { Authorization = "Bearer your-token" }
events = ["call_ringing", "call_answered", "call_hangup", "dn_state_changed"]
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | String | (required) | HTTP endpoint that receives POST requests |
| `timeout_ms` | u64 | 5000 | HTTP request timeout in milliseconds |
| `headers` | HashMap | (optional) | Custom HTTP headers sent with every request |
| `events` | Vec\<String\> | [] (all events) | Event type whitelist; if empty, all events are forwarded |

### How It Works

1. **Startup**: `src/app.rs:510-516` reads `config.rwi_webhook` and calls `start_rwi_webhook_handler()`
2. **Channel**: A `broadcast::channel` (capacity 512) connects the `RwiGateway` to the webhook handler task
3. **Gateway integration**: Every `RwiGateway` method (`send_event_to_call_owner`, `broadcast_event`, `fan_out_event_to_context`) calls `forward_to_webhook()` after dispatching to WebSocket sessions
4. **Dedup**: A ring buffer of 4096 `(call_id, sequence)` pairs prevents duplicate delivery when the same event is forwarded by multiple call owners
5. **Filtering**: If `events` is non-empty, only event types in the list are forwarded
6. **HTTP POST**: Each event is POSTed as JSON with a 5-second default timeout

### Webhook Payload Format

```json
{
  "rwi": "1.0",
  "sequence": 42,
  "timestamp": 1716212345,
  "call_id": "call-abc123",
  "event_type": "call_ringing",
  "event": {
    "call_ringing": {
      "call_id": "call-abc123"
    }
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `rwi` | String | Protocol version (`"1.0"`) |
| `sequence` | u64 | Monotonically increasing event sequence number |
| `timestamp` | u64 | Unix epoch seconds |
| `call_id` | String | Call identifier (empty string for broadcast-only events) |
| `event_type` | String | Snake_case event type name (e.g., `"call_ringing"`, `"dn_state_changed"`) |
| `event` | Object | The full RWI event payload keyed by event type |

### Event Type Names

The webhook uses snake_case event type names matching the JSON serialization of `RwiEvent` enum variants:

| Event Type | RwiEvent Variant |
|------------|------------------|
| `call_incoming` | `CallIncoming` |
| `call_ringing` | `CallRinging` |
| `call_early_media` | `CallEarlyMedia` |
| `call_answered` | `CallAnswered` |
| `call_bridged` | `CallBridged` |
| `call_unbridged` | `CallUnbridged` |
| `call_transferred` | `CallTransferred` |
| `call_transfer_accepted` | `CallTransferAccepted` |
| `call_transfer_failed` | `CallTransferFailed` |
| `call_hangup` | `CallHangup` |
| `call_no_answer` | `CallNoAnswer` |
| `call_busy` | `CallBusy` |
| `media_hold_started` | `MediaHoldStarted` |
| `media_hold_stopped` | `MediaHoldStopped` |
| `media_play_started` | `MediaPlayStarted` |
| `media_play_finished` | `MediaPlayFinished` |
| `media_stream_started` | `MediaStreamStarted` |
| `media_stream_stopped` | `MediaStreamStopped` |
| `dtmf` | `Dtmf` |
| `dtmf_collected` | `DtmfCollected` |
| `dtmf_collection_timeout` | `DtmfCollectionTimeout` |
| `record_started` | `RecordStarted` |
| `record_stopped` | `RecordStopped` |
| `record_failed` | `RecordFailed` |
| `recording_metadata_available` | `RecordingMetadataAvailable` |
| `ivr_node_entered` | `IvrNodeEntered` |
| `ivr_node_exited` | `IvrNodeExited` |
| `ivr_flow_transitioned` | `IvrFlowTransitioned` |
| `ivr_flow_completed` | `IvrFlowCompleted` |
| `ivr_step_trace` | `IvrStepTrace` |
| `dn_state_changed` | `DnStateChanged` |
| `dn_registered` | `DnRegistered` |
| `dn_unregistered` | `DnUnregistered` |
| `call_metadata_updated` | `CallMetadataUpdated` |
| `agent_state_changed` | `AgentStateChanged` |
| `queue_joined` | `QueueJoined` |
| `queue_left` | `QueueLeft` |
| `queue_overflowed` | `QueueOverflowed` |
| `queue_alert` | `QueueAlert` |
| `conference_created` | `ConferenceCreated` |
| `conference_member_joined` | `ConferenceMemberJoined` |
| `conference_member_left` | `ConferenceMemberLeft` |
| `conference_destroyed` | `ConferenceDestroyed` |
| `supervisor_mode_stopped` | `SupervisorModeStopped` |
| `parallel_originate_started` | `ParallelOriginateStarted` |
| `parallel_originate_winner` | `ParallelOriginateWinner` |
| `parallel_originate_completed` | `ParallelOriginateCompleted` |
| `parallel_originate_failed` | `ParallelOriginateFailed` |
| `sip_message_received` | `SipMessageReceived` |
| `sip_notify_received` | `SipNotifyReceived` |

### Webhook Test Endpoint

A built-in test endpoint verifies webhook connectivity:

```
POST /settings/config/proxy/rwi-webhook/test
Authorization: Bearer <admin-token>

{
  "url": "https://hook.example.com/rwi-events",
  "headers": { "Authorization": "Bearer your-token" }
}
```

Sends a test event payload and returns the HTTP response status.

### Settings UI

The RWI webhook can be configured via the admin console at **Settings → Proxy → RWI Webhook**:

```
┌─────────────────────────────────────────────┐
│ RWI Webhook                                 │
│                                             │
│ URL:   [https://hook.example.com/rwi-events]│
│ Timeout: [5000] ms                          │
│ Headers: [Authorization: Bearer ...      ]  │
│ Events:  [call_ringing                   ]  │
│          [call_answered                  ]  │
│          [call_hangup                    ]  │
│          [dn_state_changed               ]  │
│                                             │
│ [Send Test]                    [Save]       │
└─────────────────────────────────────────────┘
```

### Deduplication

Since `RwiGateway` may forward the same event through multiple dispatch paths (e.g., both `call_owner` and `fan_out_to_context`), the webhook handler maintains a dedup cache:

- **Capacity**: 4096 entries (ring buffer)
- **Key**: `(call_id, sequence)` tuple
- **Behavior**: If the same `(call_id, sequence)` pair is received within the cache window, the duplicate is silently dropped

### Example: Receiving Webhook with Python

```python
from flask import Flask, request

app = Flask(__name__)

@app.route("/rwi-events", methods=["POST"])
def handle_rwi_event():
    payload = request.json
    event_type = payload["event_type"]
    call_id = payload["call_id"]
    sequence = payload["sequence"]

    print(f"[RWI Webhook] #{sequence} {event_type} call={call_id}")

    # Access event-specific data
    event_data = payload["event"][event_type]
    if event_type == "call_ringing":
        print(f"  Call {event_data['call_id']} is ringing")
    elif event_type == "call_hangup":
        print(f"  Hangup reason: {event_data.get('reason')}")
    elif event_type == "dn_state_changed":
        print(f"  DN {event_data['dn']}: {event_data['event_name']}")

    return {"status": "ok"}, 200
```
