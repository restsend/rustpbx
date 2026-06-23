# Step Mode IVR — Build Your Custom Provider

> Use an external HTTP service to control the IVR flow step by step.  
> Call events (DTMF, timeout, audio complete, ...) are POSTed to your URL.  
> You reply with `ActionNode` — what to do next.

---

## 1. Quick Start

```bash
# 1. Start the reference Python provider
python3 examples/step_ivr_provider.py 8080

# 2. Simulate a call entering the IVR
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d '{"session_id":"call_001","caller":"1001","callee":"2000","event":{"type":"session_start"}}'

# → returns: { "type": "prompt", "file": "sounds/ivr/welcome.wav", ... }

# 3. Simulate a DTMF keypress
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d '{"session_id":"call_001","caller":"1001","callee":"2000","event":{"type":"dtmf","digit":"1"}}'
```

---

## 2. How It Works

```
Call enters IVR
      │
      ▼
POST {url}/start          ──→ your provider (session notification, fire‑and‑forget)
POST {url}  (session_start) ──→ your provider ──→ ActionNode
      │                                               │
      ▼ execute action                                │
 [play prompt]                                         │
      │                                               │
      ▼ audio complete                                 │
POST {url}  (audio_complete) ──→ your provider ────────┘
      │
      ▼
  ... repeat until terminal action (transfer / hangup / queue)
      │
      ▼
POST {url}/end            ──→ your provider (session cleanup, fire‑and‑forget)
```

| Endpoint | Called When | Expects Response? |
|----------|-------------|-------------------|
| `POST {url}` | Every IVR step | ✅ `ActionNode` |
| `POST {url}/start` | Session starts | ❌ fire-and-forget (headers are sent, body is `SessionContext`) |
| `POST {url}/end` | Session ends (any reason) | ❌ fire-and-forget (body: `{"reason": "...", "detail": "..."}` — see §6 End Reason Tags) |

---

## 3. Request Format — ProviderContext

```json
{
  "session_id": "call_abc123",
  "caller": "1001",
  "callee": "2000",
  "direction": "inbound",
  "tenant_id": null,
  "ivr_id": "proj_42",
  "variables": {
    "user_phone": "13800138000",
    "api_result": "{\"balance\": 100}"
  },
  "event": {
    "type": "dtmf",
    "digit": "1"
  }
}
```

### ProviderContext fields

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | `string` | Unique per call |
| `caller` | `string` | Calling party number |
| `callee` | `string` | Called party number |
| `direction` | `"inbound"` or `"outbound"` | Call direction |
| `tenant_id` | `string` or `null` | Tenant identifier |
| `ivr_id` | `string` or `null` | IVR project identifier for tracing |
| `variables` | `Map<string, string>` | Session variables (see §Variables) |
| `event` | `ProviderEvent` or `null` | The event that triggered this step |

### ProviderEvent types

| `type` | Extra fields | Triggered When |
|--------|--------------|----------------|
| `session_start` | (none) | Call enters the IVR |
| `dtmf` | `digit: string` | User presses a DTMF key |
| `dtmf_timeout` | (none) | Digit collection timed out with no input |
| `audio_complete` | `interrupted: bool` | Playback finished. `interrupted=true` if DTMF arrived during playback |
| `api_response` | `status: u16`, `body: json` | `api` action returned. `body` is a raw JSON value. |
| `phone_collected` | `number: string` | Phone number input collection complete |
| `recording_complete` | `url: string`, `duration_secs: u64` | Recording/voicemail capture finished |
| `input_voice` | `text: string`, `confidence: f32` | ASR recognition result (confidence 0.0–1.0) |
| `error` | `reason: string` | An execution error occurred, e.g. TTS playback failed (TTS service not configured and edge-cli fallback unavailable) — see §Error Handling |
| `dtmf_menu_invalid` | `digit: string` | Menu mode: user pressed a key not in `entries`, and no `invalid_action` was set |
| `dtmf_menu_timeout` | (none) | Menu mode: no key pressed before timeout, and no `timeout_action` was set |

---

## 4. Response — ActionNode

Every response is a JSON object with a `"type"` field. Two categories:

### Terminal Actions (IVR exits after executing)

```json
{ "type": "transfer",       "target": "2001" }
{ "type": "hangup",         "prompt": null }
{ "type": "queue",          "target": "support" }
{ "type": "voicemail",      "target": "1001" }
{ "type": "play_and_hangup","prompt": "goodbye.wav", "code": 200 }
{ "type": "jump_ivr",       "route_point": "other_ivr", "params": {"key":"val"} }
{ "type": "route_to_agent", "target": "9200", "skill_group_id": "sales" }
{ "type": "voip_bridge",    "create_room_uri": "wss://...", "headers": {...}, "timeout_ms": 30000 }
```

### Non-terminal Actions (execute, wait for next event, then call provider again)

```json
{ "type": "prompt",        "file": "hello.wav",    "interruptible": true }
{ "type": "dtmf_menu",     "greeting": "menu.wav", "entries": {...} }
{ "type": "collect_dtmf",  "min_digits": 1,        "max_digits": 4, "timeout_ms": 5000 }
{ "type": "input_phone",   "prompt": "enter.wav" }
{ "type": "input_voice",   "scene": "order",       "timeout_ms": 8000 }
{ "type": "api",           "url": "https://api.example.com", "method": "POST", "timeout": 10 }
{ "type": "torecord",      "prompt": "leave_msg.wav", "beep": true }
```

### Transparent Passthrough Fields

Every `ActionNode` response can include three optional top-level fields for node identification and data passthrough:

```json
{
  "type": "prompt",
  "file": "hello.wav",
  "step_id": "1000602002200750100",
  "step_name": "欢迎语",
  "extra": {
    "tenantId": "didi",
    "gvpFlow": "CTCDaiJiaKeFu",
    "callPath": "F_11,F",
    "businessType": "6",
    "customerType": "1",
    "routePoint": "39325"
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `step_id` | `string` or null | Node identifier — stored and emitted in `ivr_step_trace` events as `step_id` |
| `step_name` | `string` or null | Node name — stored and emitted as `step_name` |
| `extra` | `JSON Object` or null | Transparent passthrough data — provider returns the complete object each time; RustPBX stores it and includes it verbatim in every subsequent `ivr_step_trace` event |

> **Passthrough behavior**: After `session_start`, the provider should include `extra` in every response. RustPBX stores the latest `extra` value and echoes it in all `ivr_step_trace` events until the provider updates it. The `step_id` and `step_name` from the most recent provider response are used for the current step's trace.

### Next Chaining

Non-terminal actions can include a `next` field to chain multiple actions without a round-trip:

```json
{
  "type": "prompt",
  "file": "hello.wav",
  "interruptible": true,
  "next": {
    "type": "dtmf_menu",
    "greeting": "menu.wav",
    "timeout_ms": 5000,
    "entries": {
      "1": { "type": "transfer", "target": "2001" },
      "2": { "type": "queue", "target": "support" }
    }
  }
}
```

RustPBX plays the prompt → on audio complete → automatically executes the dtmf_menu (no POST to your provider between them).

---

## 5. Action Types — Reference

### Terminal actions

| type | Purpose | Required | Optional | Notes |
|------|---------|----------|----------|-------|
| `transfer` | Blind‑transfer the call to a SIP URI or extension | `target: string` | — | The call leaves the IVR permanently |
| `queue` | Send the caller into an ACD queue | `target: string` | `return_to_ivr: bool` | When `return_to_ivr=true`, the call returns to IVR if no agent answers |
| `voicemail` | Forward the caller to a user's voicemail | `target: string` | — | The call leaves the IVR |
| `hangup` | Terminate the call | — | `prompt: string or null` | If `prompt` is set, plays audio before hanging up |
| `play_and_hangup` | Play an announcement then hang up with a SIP status code | — | `prompt: string or null`, `code: u16 or null` | `code` is the SIP response code (e.g. 486 busy, 404 not found) |
| `jump_ivr` | Jump to another named IVR route point | `route_point: string` | `params: Map<string,string>` | The new IVR receives `params` as session variables |
| `route_to_agent` | Route the call to a human agent via skill‑based routing | `target: string` | `skill_group_id: string`, `key_id: string`, `channel_code: string` | Uses the contact‑center routing engine |
| `voip_bridge` | Bridge call audio to an external WebSocket endpoint as raw PCM16 | `create_room_uri: string` | `headers: Map<string,string>`, `timeout_ms: u64` | See [voip_bridge_en.md](voip_bridge_en.md) |

### Non-terminal actions

| type | Purpose | Required | Optional | Notes |
|------|---------|----------|----------|-------|
| `prompt` | Play an audio file or TTS | — | `file: string`, `tts_text: string`, `tts_voice: string`, `interruptible: bool` (default: false), `record_name_list: string` | `file` and `tts_text` are mutually exclusive. Next event: `audio_complete` |
| `dtmf_menu` | Play a greeting and wait for DTMF, resolve locally via `entries` | — | `greeting: string`, `greeting_text: string`, `entries: Map<string,ActionNode>`, `timeout_ms: u64` (default: 5000), `max_retries: u32` (default: 3), `timeout_action: ActionNode`, `invalid_action: ActionNode` | If `entries` is non‑empty, DTMF is handled locally. Otherwise DTMF is forwarded to provider as `dtmf_menu_invalid`/`dtmf_menu_timeout`. Next event: `dtmf`, `dtmf_timeout`, or terminal action matched in entries |
| `collect_dtmf` | Collect a fixed number of DTMF digits | — | `min_digits: usize` (default: 3), `max_digits: usize` (default: 4), `timeout_ms: u64` (default: 5000), `terminator: string` (e.g. `"#"`), `prompt: string` | Result stored in session variable `dtmf_input`. Next event: `dtmf` (per digit) or `dtmf_timeout` |
| `input_phone` | Collect a phone number (11 digits by default) | — | `prompt: string`, `min_digits: usize` (default: 11), `max_digits: usize` (default: 11) | Result stored in session variable `phone_number`. Next event: `phone_collected` |
| `input_voice` | ASR voice input | `scene: string` | `timeout_ms: u64` (default: 5000) | If ASR is unavailable, returns a `WaitFor` to the IVR executor, which then sends an `error` event to the provider. Next event: `input_voice` |
| `api` | Call an external HTTP API | `url: string` | `method: string` (default: `"GET"`), `headers: Map<string,string>`, `variables: string` (comma‑separated variable names to pass), `timeout: u64` (default: 10, seconds), `get_dynamic_tree: bool` | The response body is returned as `api_response.body`. Next event: `api_response` |
| `torecord` | Capture a voice recording / voicemail message | — | `prompt: string`, `beep: bool` (default: false), `max_duration_secs: u32 or null` | Recording is saved to `recordings/{session_id}/{timestamp}.wav`. Next event: `recording_complete` |

### DtmfMenu local resolution (in step mode)

When `entries` is non‑empty, DTMF is resolved locally without calling your provider:

```
User presses a key
  ├── key in entries?       → execute mapped ActionNode immediately
  ├── invalid_action set?   → play it, retry; if retries exhausted, execute invalid_action
  └── no invalid_action?    → forward DTMF to provider as `dtmf_menu_invalid` event
```

When `timeout_action` is set, timeout executes it; otherwise sends `dtmf_menu_timeout` to provider.

### DTMF event filtering (early DTMF)

RustPBX filters out DTMF events that arrive when the IVR is **not expecting user input**. This prevents stray key presses during audio playback from derailing the flow.

| IVR State | DTMF Behavior |
|-----------|---------------|
| Playing a `prompt` (non-interruptible) | **Ignored** — the digit is silently dropped |
| Playing a `prompt` (interruptible) | Interrupts playback, digit forwarded to provider |
| `dtmf_menu` greeting playing | Resolved locally via `entries` (barge-in) |
| `dtmf_menu` greeting finished (`awaiting_dtmf`) | Resolved locally or forwarded to provider |
| `collect_dtmf` / `input_phone` in progress | Consumed by the digit collector |
| Any terminal action executing | **Ignored** |

> **Note:** If your provider expects to receive every DTMF digit regardless of state, make sure the preceding action is `dtmf_menu` or `collect_dtmf`, not a plain `prompt`. A `prompt` followed by `collect_dtmf` via `next` chaining is the recommended pattern — the `prompt` plays first, and only after audio completes does the `collect_dtmf` step begin accepting digits.

---

## 6. Building a Provider

### State Machine Pattern (Python)

Your provider is a per-call state machine. The reference implementation at `examples/step_ivr_provider.py`:

```python
class IvrSession:
    def __init__(self, caller, callee):
        self.step = "start"
        self.retries = 0

    def next_action(self, event) -> dict:
        ev_type = event.get("type", "")

        # Step 1: initial greeting + DTMF menu (chained in one response)
        if self.step == "start":
            self.step = "menu"
            return {
                "type": "prompt",
                "file": "sounds/ivr/welcome.wav",
                "interruptible": True,
                "next": {
                    "type": "dtmf_menu",
                    "greeting": "sounds/ivr/menu.wav",
                    "timeout_ms": 5000,
                    "entries": {
                        "1": { "type": "transfer", "target": "2001" },
                        "2": { "type": "queue", "target": "support" },
                        "0": { "type": "transfer", "target": "operator" },
                    },
                },
            }

        # Step 2: handle DTMF from the menu
        if self.step == "menu":
            if ev_type == "dtmf":
                digit = event.get("digit", "")
                if digit == "1":  return {"type": "transfer", "target": "2001"}
                if digit == "2":  return {"type": "queue", "target": "support"}
                if digit == "0":  return {"type": "transfer", "target": "operator"}
                self.retries += 1
                if self.retries >= 3:
                    return {"type": "hangup"}
                return {"type": "prompt", "file": "sounds/ivr/invalid.wav",
                        "next": {"type": "repeat"}}
            if ev_type == "dtmf_timeout":
                return {"type": "prompt", "file": "sounds/ivr/timeout.wav",
                        "next": {"type": "repeat"}}

        return {"type": "hangup"}  # fallback
```

### HTTP Server Skeleton

```python
from http.server import HTTPServer, BaseHTTPRequestHandler
import json

class Handler(BaseHTTPRequestHandler):
    sessions = {}

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        sid = body.get("session_id", "")

        if self.path == "/ivr/step/start":
            self.sessions[sid] = IvrSession(body.get("caller"), body.get("callee"))
            self._json(200, {"status": "ok"})

        elif self.path == "/ivr/step/end":
            self.sessions.pop(sid, None)
            self._json(200, {"status": "ok"})

        else:  # /ivr/step (main endpoint)
            session = self.sessions.get(sid)
            if not session:
                session = self.sessions[sid] = IvrSession("unknown", "unknown")
            node = session.next_action(body.get("event", {}))
            self._json(200, node)

    def _json(self, status, data):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())
```

### Three Endpoints

Your provider serves three POST endpoints. RustPBX calls them automatically:

**`POST {url}`** (every step) — Receive `ProviderContext`, return `ActionNode`.
- Headers: `Content-Type: application/json` + any custom headers from config.
- Request body: `ProviderContext` (see §3).
- Response body: `ActionNode` (see §4).
- Retry: if the request fails (timeout, 5xx, network error), RustPBX retries up to `max_retries` times.

**`POST {url}/start`** (session start) — Optional notification. No response expected.
- Body: `SessionContext` (`session_id`, `caller`, `callee`, `direction`, `tenant_id`, `ivr_id`).
- Fire-and-forget: no retry, response ignored.

**`POST {url}/end`** (session end) — Notification sent when the IVR session ends for any reason. No response expected.
- Fire-and-forget: no retry, response ignored.
- Body is structured JSON with `reason` (machine-readable tag) and optional `detail`:

```json
{
  "session_id": "call_abc123",
  "reason": "transfer",
  "detail": "2001"
}
```

#### End reason tags

| `reason` | `detail` | Triggered When |
|----------|----------|----------------|
| `"normal"` | `null` | IVR completed all steps without transfer (e.g. `hangup` action) |
| `"transfer"` | target (e.g. `"2001"`) | Call transferred to an agent or extension via `transfer` action |
| `"transfer_to_queue"` | queue name (e.g. `"support"`) | Call sent to an ACD queue via `queue` action |
| `"transfer_to_ivr"` | route point (e.g. `"main"`) | Call jumped to another IVR via `jump_ivr` action |
| `"hangup"` | `null` | System (PBX) initiated hangup |
| `"user_hangup"` | `null` | User / remote party hung up |
| `"error"` | error message | Error during IVR execution (e.g. provider unreachable after all retries) |

> **Example responses** for common scenarios:
>
> User hangs up during menu:
> ```json
> { "session_id": "call_1", "reason": "user_hangup", "detail": null }
> ```
>
> Caller presses `1` → transfer to agent `2001`:
> ```json
> { "session_id": "call_1", "reason": "transfer", "detail": "2001" }
> ```
>
> IVR flow completed, system hangs up:
> ```json
> { "session_id": "call_1", "reason": "normal" }
> ```
> *(when `detail` is `null` it is omitted from the JSON body)*

**`POST {url}/dtmf-match`** (optional) — Called when a `DtmfMenu` entry matches locally and the action is executed without a round-trip. Fire-and-forget, no response expected.
- Body: `{"digit": "1", "action": {"type": "transfer", "target": "2001"}}`
- This keeps your provider informed about user input even though no `ProviderEvent` is sent.

---

## 7. Variable Substitution

Any `$var_name$` in string fields is replaced from session variables:

```json
{ "type": "transfer", "target": "$agent_extension$" }
{ "type": "api", "url": "https://api.example.com/status/$session_id$" }
```

### Pre-defined variables

| Variable | Source |
|----------|--------|
| `session_id` | Session identifier |
| `caller` | Calling party number |
| `callee` | Called party number |
| `direction` | Call direction |
| `tenant_id` | Tenant identifier |
| `dtmf_input` | Result from `collect_dtmf` |
| `phone_number` | Result from `input_phone` |
| `api_status` | HTTP status from `api` action |
| `api_result` | Response body from `api` action |

Any variables returned in `ProviderContext.variables` by the provider are also available.

---

## 8. Error Handling & Retry

| Scenario | Behavior |
|----------|----------|
| **Provider HTTP timeout** (per‑request timeout = `retry.timeout_ms`, default 1000ms) | Retry, up to `retry.max_retries` (default 3). Between retries: 100ms sleep. |
| **Provider returns 5xx** | Same as timeout — retry loop. |
| **All retries exhausted** | Execute `retry.fallback` (default: `{"type":"hangup","prompt":"sounds/error.wav"}`). |
| **Provider returns invalid JSON / unknown action type** | Record trace error → play `sounds/error.wav` → hangup. |
| **Provider returns tree‑mode only action** (`repeat`/`back`/`play`/`menu`/`collect_extension`/`collect`/`webhook`) | Not executed, returns error. |

### TTS Audio Fallback

When a `prompt` action contains `tts_text` but no TTS service is configured:

1. RustPBX attempts to synthesize via **edge-cli** (Microsoft Edge TTS CLI) as a built-in fallback.
2. If edge-cli succeeds, the audio plays normally.
3. If edge-cli is not available or fails, RustPBX sends an `error` event (`{"type":"error","reason":"TTS service not available"}`) to your provider. Your provider should handle this event and return a fallback action (e.g. a `file`-based prompt, or a different flow).

**Important:** Do not return another `tts_text`-based prompt in response to an `error` event — this would cause the same TTS failure again, and RustPBX will send another `error` event (no infinite loop due to the error → fallback → terminal action chain).

---

## 9. Configuration

### Route params (used at runtime)

```json
{
  "mode": "step",
  "url": "http://localhost:8080/ivr/step",
  "headers": {
    "Authorization": "Bearer token123"
  },
  "retry": {
    "max_retries": 5,
    "timeout_ms": 2000,
    "fallback": { "type": "transfer", "target": "operator" }
  },
  "name": "my-step-ivr"
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | — | Provider HTTP endpoint (POST) |
| `headers` | `Map<string,string>` | `{}` | Custom HTTP headers sent on every provider call |
| `retry.max_retries` | u32 | `3` | Max retry attempts |
| `retry.timeout_ms` | u64 | `1000` | Per‑request timeout in **milliseconds** |
| `retry.fallback` | ActionNode | `{"type":"hangup","prompt":"sounds/error.wav"}` | Action to execute when all retries fail |
| `name` | string | `"step_ivr"` | Display name for tracing |

### Published step.json (IVR Editor creates this, for reference)

```json
{
  "mode": "step",
  "name": "My IVR",
  "url": "https://provider.example.com/ivr/step",
  "headers": { "Authorization": "Bearer token123" },
  "retry": {
    "max_retries": 3,
    "timeout_ms": 1000
  }
}
```

### Route config (TOML)

```toml
[[routes]]
name = "my_step_ivr"
to_user = "*99"
app = "ivr"
app_params = { mode = "step", url = "http://localhost:8080/ivr/step" }
```

---

## 10. Testing & Debugging

### curl

```bash
SESSION="test_$(date +%s)"
# Session start
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"$SESSION\",\"caller\":\"1001\",\"callee\":\"2000\",\"event\":{\"type\":\"session_start\"}}"

# Simulate DTMF
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"$SESSION\",\"event\":{\"type\":\"dtmf\",\"digit\":\"1\"}}"

# Simulate audio complete
curl -X POST http://localhost:8080/ivr/step \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"$SESSION\",\"event\":{\"type\":\"audio_complete\",\"interrupted\":false}}"
```

### Traces (RustPBX side)

Every step is recorded. View in: IVR Editor → Debug → select session. Each entry shows the exact `ProviderContext` sent and the `ActionNode` returned, with timing.

RWI subscribers receive trace entries in real-time (event type: `ivr_step_trace`).

### Reference Implementation

`examples/step_ivr_provider.py` — complete Python provider, zero external dependencies. Covers: session state machine, Prompt → DtmfMenu → Transfer/Queue/Hangup, DTMF timeout handling, invalid digit retry with 3-strike hangup.
