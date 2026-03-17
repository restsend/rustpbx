# RWI Common Use Cases

This document organizes common call scenarios for RWI as a reference for E2E testing and development.

---

## Scenario 1: Basic Outbound + Bridge

**Scenario**: Call customer, bridge to agent after customer answers

```
Client → call.originate(leg_a, customer)
         ← call.answered(leg_a)

Client → call.originate(leg_b, agent)
         ← call.answered(leg_b)

Client → call.bridge(leg_a, leg_b)
         ← call.bridged(leg_a, leg_b)
```

**Code Example**:
```json
{"action": "call.originate", "params": {"call_id": "leg_a", "destination": "sip:1234567890@trunk", "caller_id": "2000", "timeout_secs": 30}}
{"action": "call.originate", "params": {"call_id": "leg_b", "destination": "sip:1001@local", "caller_id": "2000", "timeout_secs": 30}}
{"action": "call.bridge", "params": {"leg_a": "leg_a", "leg_b": "leg_b"}}
```

---

## Scenario 2: Attended Transfer

**Scenario**: Customer is on a call, needs transfer to another agent

```
Client → call.transfer.attended(call_id=leg_a, target=sip:agent2@local)
         ← response: success, consultation_call_id: "consult-leg_a-xxx"

         ← call.answered(consult-leg_a-xxx)  // agent2 answers

Client → call.transfer.complete(call_id=leg_a, consultation_call_id=consult-leg_a-xxx)
         ← call.bridged  // leg_a and agent2 bridged, leg_a's previous party hung up
```

**Full Flow**:
```json
// 1. Customer (leg_a) is already on call with agent1

// 2. Initiate attended transfer to agent2
{"action": "call.transfer.attended", "params": {
  "call_id": "leg_a",
  "target": "sip:agent2@local",
  "timeout_secs": 30
}}

// 3. After agent2 answers, complete the transfer
{"action": "call.transfer.complete", "params": {
  "call_id": "leg_a",
  "consultation_call_id": "consult-leg_a-xxx"
}}

// Or cancel transfer, restore call with agent1
{"action": "call.transfer.cancel", "params": {
  "consultation_call_id": "consult-leg_a-xxx"
}}
```

---

## Scenario 3: Agent Swap

**Scenario**: During a call, replace current agent with another agent

**Method A: Hangup then bridge**
```json
// leg_a(customer) and leg_b(agent1) are already bridged

// 1. Hang up agent1
{"action": "call.hangup", "params": {"call_id": "leg_b"}}

// 2. Call new agent
{"action": "call.originate", "params": {
  "call_id": "leg_c",
  "destination": "sip:agent2@local",
  "context": "default"
}}

// 3. Bridge to new agent
{"action": "call.bridge", "params": {"leg_a": "leg_a", "leg_b": "leg_c"}}
```

**Method B: Direct transfer (recommended)**
```json
// leg_a(customer) and leg_b(agent1) are already bridged

// 1. Initiate new attended transfer to agent2
{"action": "call.transfer.attended", "params": {
  "call_id": "leg_a",
  "target": "sip:agent2@local"
}}

// 2. Complete transfer (automatically hangs up leg_b)
{"action": "call.transfer.complete", "params": {
  "call_id": "leg_a",
  "consultation_call_id": "consult-leg_a-xxx"
}}
```

---

## Scenario 4: Continue Transfer After Bridge

**Scenario**: Two legs are already bridged, need to continue transfer to third party (IVR/Queue/Voicemail)

**Conclusion**: Supported. You can initiate another attended transfer on an already bridged call.

```json
// leg_a and leg_b are already bridged

// 1. Initiate attended transfer to new target (can be another agent, IVR, Queue or voicemail)
{"action": "call.transfer.attended", "params": {
  "call_id": "leg_a",
  "target": "sip:agent2@local"  // or sip:queue@local, sip:voicemail@local
}}

// 2. Complete transfer
{"action": "call.transfer.complete", "params": {
  "call_id": "leg_a",
  "consultation_call_id": "consult-leg_a-xxx"
}}

// Result: leg_a disconnects from leg_b, bridges with new target
```

**Examples for different targets**:

| Target Type | target Example |
|------------|---------------|
| Agent | `sip:1001@local` |
| IVR | `sip:ivr_main@local` (requires dialplan config) |
| Queue | `sip:support_queue@local` (requires queue addon) |
| Voicemail | `sip:voicemail@local` |

---

## Scenario 5: Hold Music + Parallel Outbound (Contact Center)

**Scenario**: Customer waits on hold music while dialing multiple agents in parallel, first to answer wins

```
Client → call.originate(leg_a, customer)
         ← call.answered(leg_a)

Client → media.play(leg_a, hold_music.wav, loop=true)
         ← media.play.started

Client → call.originate(leg_b, agent1)
Client → call.originate(leg_c, agent2)
         ← call.answered(leg_c)  // agent2 answers first

Client → call.hangup(leg_b)  // cancel agent1
Client → media.stop(leg_a)   // stop hold music
Client → call.bridge(leg_a, leg_c)
```

```json
{"action": "call.originate", "params": {"call_id": "leg_a", "destination": "sip:customer@local"}}
{"action": "media.play", "params": {"call_id": "leg_a", "source": {"type": "file", "uri": "hold.wav"}, "loop": true}}
{"action": "call.originate", "params": {"call_id": "leg_b", "destination": "sip:agent1@local"}}
{"action": "call.originate", "params": {"call_id": "leg_c", "destination": "sip:agent2@local"}}
{"action": "call.hangup", "params": {"call_id": "leg_b"}}
{"action": "media.stop", "params": {"call_id": "leg_a"}}
{"action": "call.bridge", "params": {"leg_a": "leg_a", "leg_b": "leg_c"}}
```

---

## Scenario 6: Play IVR Prompt and Collect DTMF

**Scenario**: Call customer, play voice prompt, transfer based on keypress

```json
// 1. Call customer
{"action": "call.originate", "params": {"call_id": "leg_a", "destination": "sip:customer@local"}}

// 2. Play IVR prompt, interruptible
{"action": "media.play", "params": {
  "call_id": "leg_a",
  "source": {"type": "file", "uri": "welcome.wav"},
  "interrupt_on_dtmf": true
}}
// ← media.play.started
// ← media.play.finished (after customer presses key)
// ← call.dtmf (keypress event)

// 3. Transfer to queue based on customer choice
{"action": "call.transfer.attended", "params": {
  "call_id": "leg_a",
  "target": "sip:sales@local"  // or dynamically select based on keypress
}}
```

---

## Scenario 7: Supervisor Listen/Whisper/Barge

**Scenario**: Supervisor monitors agent's call with customer, can whisper or barge in

```json
// Assume leg_a(customer) and leg_b(agent) are already bridged

// 1. Supervisor joins in listen mode
{"action": "call.originate", "params": {
  "call_id": "supervisor_leg",
  "destination": "sip:supervisor@local"
}}

// 2. Listen mode (hear only, no speak)
{"action": "supervisor.listen", "params": {
  "supervisor_call_id": "supervisor_leg",
  "target_call_id": "leg_b"
}}
// ← supervisor.listen.started

// 3. Switch to whisper mode (customer cannot hear supervisor)
{"action": "supervisor.whisper", "params": {
  "supervisor_call_id": "supervisor_leg",
  "target_call_id": "leg_b"
}}

// 4. Switch to barge mode (three-way call)
{"action": "supervisor.barge", "params": {
  "supervisor_call_id": "supervisor_leg",
  "target_call_id": "leg_b"
}}

// 5. Stop supervisor mode
{"action": "supervisor.stop", "params": {
  "supervisor_call_id": "supervisor_leg",
  "target_call_id": "leg_b"
}}
// ← supervisor.mode.stopped
```

---

## Scenario 8: Recording Control

**Scenario**: Start/pause/stop recording during a call

```json
// 1. Start recording
{"action": "record.start", "params": {
  "call_id": "leg_a",
  "mode": "mixed",
  "beep": true,
  "max_duration_secs": 3600
}}
// ← record.started, recording_id: "rec_xxx"

// 2. Pause recording (e.g., customer requests confidentiality)
{"action": "record.pause", "params": {"call_id": "leg_a"}}
// ← record.paused

// 3. Resume recording
{"action": "record.resume", "params": {"call_id": "leg_a"}}
// ← record.resumed

// 4. Stop recording
{"action": "record.stop", "params": {"call_id": "leg_a"}}
// ← record.stopped, recording_id: "rec_xxx", duration_secs: 120
```

---

## Scenario 9: Queue Control

**Scenario**: Add call to queue, remove from queue

```json
// 1. Add existing call to queue
{"action": "queue.enqueue", "params": {
  "call_id": "leg_a",
  "queue_id": "support_queue",
  "priority": 5,
  "max_wait_secs": 300
}}
// ← queue.joined

// 2. Hold in queue
{"action": "queue.hold", "params": {"call_id": "leg_a"}}
// ← queue.hold

// 3. Unhold from queue
{"action": "queue.unhold", "params": {"call_id": "leg_a"}}
// ← queue.unhold

// 4. Dequeue (agent answers)
{"action": "queue.dequeue", "params": {
  "call_id": "leg_a",
  "agent_id": "1001"
}}
// ← queue.agent_connected
```

---

## Scenario 10: SIP Message Sending

**Scenario**: Send SIP MESSAGE or NOTIFY during a call

```json
// Send SIP MESSAGE (text message)
{"action": "sip.message", "params": {
  "call_id": "leg_a",
  "content_type": "text/plain",
  "body": "Your ticket number is 12345"
}}

// Send SIP NOTIFY (e.g., MWI)
{"action": "sip.notify", "params": {
  "call_id": "leg_a",
  "event": "check-sync",
  "content_type": "application/simple-message-summary",
  "body": "Messages-Waiting: yes"
}}

// SIP OPTIONS ping (keepalive)
{"action": "sip.options_ping", "params": {"call_id": "leg_a"}}
```

---

## Scenario 11: Call Hold/Unhold

**Scenario**: Put customer on hold, then resume

```json
// Hold call
{"action": "call.hold", "params": {
  "call_id": "leg_a",
  "music": "hold.wav"  // optional custom hold music
}}
// ← media.hold.started

// Unhold
{"action": "call.unhold", "params": {"call_id": "leg_a"}}
// ← media.hold.stopped
```

---

## Scenario 12: Inbound Call Answer

**Scenario**: Receive inbound call and answer

```json
// 1. Subscribe to contexts
{"action": "session.subscribe", "params": {"contexts": ["sales", "support"]}}

// 2. Wait for incoming event
// ← {"event": "call.incoming", "call_id": "c_xxx", "context": "sales", "data": {...}}

// 3. Answer
{"action": "call.answer", "params": {"call_id": "c_xxx"}}
// ← call.answered

// 4. Or reject
{"action": "call.reject", "params": {"call_id": "c_xxx", "reason": "busy"}}
// ← call.hangup
```

---

## Error Handling Examples

```json
// Call not found
{"action": "call.hangup", "params": {"call_id": "nonexistent"}}
// ← {"response": "error", "error": {"code": "call_not_found", "message": "..."}}

// No permission
{"action": "supervisor.listen", "params": {...}}
// ← {"response": "error", "error": {"code": "forbidden", "message": "scope required: supervisor.control"}}

// Call already owned by another client
{"action": "call.answer", "params": {"call_id": "c_xxx"}}
// ← {"response": "error", "error": {"code": "already_owned", "message": "..."}}
```

---

## Command Quick Reference

| Category | Command | Description |
|----------|---------|-------------|
| Outbound | `call.originate` | Initiate outbound call |
| Inbound | `call.answer` / `call.reject` | Answer/reject inbound call |
| Bridge | `call.bridge` / `call.unbridge` | Bridge/unbridge two calls |
| Transfer | `call.transfer` (blind) | Blind transfer |
| Transfer | `call.transfer.attended` | Attended transfer (hold original, create consultation) |
| Transfer | `call.transfer.complete` | Complete attended transfer |
| Transfer | `call.transfer.cancel` | Cancel attended transfer |
| Hold | `call.hold` / `call.unhold` | Call hold/unhold |
| Media | `media.play` / `media.stop` | Play audio/stop playback |
| Recording | `record.start/pause/resume/stop` | Recording control |
| Queue | `queue.enqueue/dequeue/hold/unhold` | Queue control |
| Supervisor | `supervisor.listen/whisper/barge/stop` | Listen/whisper/barge/stop |
| SIP | `sip.message/notify/options_ping` | SIP messages |
| Session | `session.subscribe/unsubscribe/list_calls` | Session management |
