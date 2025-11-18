# IVR Design Proposal for `rustpbx`

## Goals
- Provide a deterministic, scriptable Interactive Voice Response (IVR) engine that can be invoked from routing rules, queues, or webhook-driven automations.
- Allow administrators to configure prompts, collect DTMF input, branch by digits, trigger downstream actions (transfer, queue, webhook, playback, nested IVR), and define timeout/invalid fallbacks.
- Keep the design data-driven so plans can be stored in config files (generated TOML), the database, or provided ad-hoc via APIs.
- Ensure observability (per-step tracing, metrics) and safe failure behavior so calls never get stuck.

## Core Requirements
1. **Plan Composition**
   - IVR plan is a DAG of steps; each step defines prompts, input expectations, and next actions.
   - Support reusable subplans/nested IVRs.
2. **Prompt Playback**
   - Play audio files or synthesize TTS via existing `SynthesisOption`.
   - Permit optional barge-in (interrupt prompt when DTMF received).
3. **DTMF Input**
   - Collect fixed-length or variable-length digits with configurable timeout.
   - Support regex validation and capture groups for templated targets (e.g., `{1}` substitution in SIP URIs or webhook payloads).
   - Provide retry counters plus `invalid` and `timeout` branches.
4. **Actions**
   - Transfer to SIP targets/extensions/queues/agents.
   - Invoke HTTP webhook (sync ACK before continuing or async fire-and-forget).
   - Trigger media playback (e.g., announcements) and optionally hang up.
   - Jump to another IVR step or plan (nested). 
5. **Integration Points**
   - Routing layer: `RouteRule.action` can reference an IVR plan ID instead of trunks.
   - Queue fallback / FailureAction: allow entering an IVR when queue times out.
   - API/websocket: expose telemetry and control for console UI.
6. **Non-Functional**
   - Resilient if prompts missing, HTTP errors, or invalid user input.
   - Traceable: log entered digits, path chosen, latency per step (with PII-safe masks).
   - Testable: unit tests for plan execution, DTMF parsing, and failure handling.

## Architecture Overview
```
Routing Rule / Queue / Webhook
          │
          ▼
    IVR Engine (Call Task)
          │
 ┌────────┴────────┐
 │ Prompt Player   │—streams audio/tts, handles barge-in
 │ Input Collector │—captures digits/timeouts
 │ Action Runner   │—transfer/webhook/etc
 └────────┬────────┘
          ▼
  Next Step or Exit
```
The engine runs within the existing call task (same async runtime as dialogs) so no extra signaling channel is required. Each active IVR keeps an `IvrSessionState` attached to `ActiveCall` metadata.

## Data Model
Add to `src/call/mod.rs` (or new module `call/ivr.rs`):
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrPlan {
    pub id: String,
    pub version: u32,
    pub entry_step: String,
    pub steps: HashMap<String, IvrStep>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IvrStep {
    Prompt(PromptStep),
    Input(InputStep),
    Action(ActionStep),
    Branch(BranchStep),
}
```
Supporting structs:
- `PromptStep { prompts: Vec<PromptMedia>, allow_barge_in: bool, next: Option<String> }`
- `PromptMedia { source: PromptSource, locale: Option<String>, loop_count: Option<u8> }`
- `PromptSource = File { path }, Tts { text, voice, option }, Stream { url }`
- `InputStep { min_digits, max_digits, regex, timeout_ms, attempt_limit, on_valid, on_invalid, on_timeout }`
- `ActionStep` variants: `Transfer(TransferAction)`, `Queue(QueueAction)`, `Webhook(WebhookAction)`, `Playback(PlaybackAction)`, `Hangup`, `Goto { step_id }`.
- `BranchStep { branches: Vec<Branch>, default: Option<String> }` where `Branch` supports exact digit string, prefix, or regex matches.
- `StepAvailability { calendar: String, when_closed: StepTarget, allow_override: bool }` referencing plan-level `WorkingCalendar` records (defined below).

### Dialplan integration
Dialplans now compose an explicit `DialplanFlow` chain that describes how a call progresses (queue → targets → IVR, etc.). Helper builders wire flows in the expected order:

```rust
let dialplan = Dialplan::new(session_id, req, DialDirection::Inbound)
   .with_queue(queue_plan)
   .with_targets(DialStrategy::Sequential(locations))
   .with_ivr(
      DialplanIvrConfig::from_plan_id("main_menu")
         .insert_variable("caller", caller_uri)
         .set_availability_override(false),
   );

if dialplan.has_ivr() {
   // proxycall will execute IVR flow as the final stage
}
```

`DialplanIvrConfig` can reference an existing plan by id or embed an inline `IvrPlan`. Once attached via `with_ivr`, the dialplan is considered non-empty even without SIP targets, and `dialplan.ivr_config()` exposes the configuration for runtime consumers.

### Working Hours & Calendars
- Each plan can optionally declare one or more `WorkingCalendar` entries under `plan.calendars`.
- `WorkingCalendar` is composed of:
   - `timezone`: Olson TZ string (e.g., `Asia/Shanghai`).
   - `weekly`: list of `{ days: ["mon","fri"], start: "09:00", end: "18:00" }` windows; multiple windows allowed per day (e.g., lunch breaks).
   - `overrides`: optional explicit date rules `{ date: "2025-02-10", start: "10:00", end: "16:00" }` for holidays/extended hours.
   - `closed`: list of full-day closures `{ date: "2025-10-01" }`.
- Any `PromptStep`, `InputStep`, `BranchStep`, or `ActionStep` may include an `availability: StepAvailability` block:
   - `calendar`: calendar id to evaluate.
   - `when_closed`: `StepTarget` describing what to do when the calendar reports "closed" (e.g., goto a voicemail tree, play after-hours message, hang up).
   - `allow_override`: optional bool letting runtime force entry (default `false`). Useful for admin override/testing.
- Evaluation happens before executing the step body. If closed, `when_closed` transition fires immediately, ensuring after-hours logic shares the same tooling (prompts, actions, etc.).
- This design allows building distinct “working hours” vs “after hours” flows with minimal duplication while keeping scheduling centralized.

### State Tracking
`IvrSessionState` keeps:
- `current_step_id`
- `input_buffer`
- `attempt_counters`
- `started_at`, `last_input_at`
- `history: Vec<IvrTraceEvent>` for logging/observability.
This state lives on `ActiveCall.extra_state` and is serialized if we need to persist (e.g., across restarts) via `serde_json`.

## Execution Flow
1. **Entry**: routing hands control to IVR engine with `IvrPlan` reference and call context (caller/callee URIs, media channels, `CommandSender`).
2. **Prompt**: `PromptStep` sequentially plays each media item. For TTS we reuse existing `Command::Tts`; for files we reuse `Command::Play`. `allow_barge_in` wires to DTMF handler to interrupt playback.
3. **Input**: `InputStep` arms DTMF listener. Each digit extends `input_buffer` until `max_digits` reached, `#` terminator, or timeout.
4. **Validation**: Evaluate regex / branch map. On success, dispatch `on_valid` (usually `Goto` or `Action`). On failure/timeouts use fallback steps and increment attempts; once exhausted go to `fallback_step` or hangup.
5. **Action**: Execute synchronous actions:
   - `Transfer`: build `DialRequest` (could reuse dialplan) and exit IVR.
   - `Queue`: create queue task (maybe `call::queue::enqueue`). Optionally return to IVR after queue.
   - `Webhook`: send HTTP request (use existing `handler::llmproxy` HTTP client). Optionally parse response for `next_step` or `playback` instructions.
   - `Playback`/`Hangup`: self-explanatory.
6. **Termination**: The IVR completes when a terminal action occurs or `next_step` is `None`. The engine returns a `CallFlowResult` (transfer target, queue id, or hangup reason).

## Configuration & Storage
### File-based (generated TOML)
Add `config/ivr/ivr.generated.toml` with schema:
```toml
[[plans]]
id = "main-menu"
entry_step = "welcome"

[[plans.steps]]
name = "welcome"
type = "prompt"
prompts = [ { file = "prompts/welcome.wav" } ]
next = "collect_ext"

[[plans.steps]]
name = "collect_ext"
type = "input"
min_digits = 1
max_digits = 4
regex = "^(0|[1-9][0-9]{2,3})$"
on_valid = { goto = "route_digit" }
on_invalid = { goto = "invalid_prompt" }
on_timeout = { goto = "timeout_prompt" }
```
A generator tool (similar to routes/trunks) writes this file; console UI edits translate to the schema.

### Database
Add tables:
- `ivr_plans (id, name, version, description, entry_step, definition_json, updated_at)`
- `ivr_steps` optional if we want normalized storage, otherwise keep JSON blob.
Plan distribution: the router loads plans into memory cache on deploy; hot reload via `/api/ivr/reload`.

## Routing Integration
- Extend `RouteAction` with `ivr_plan: Option<String>`; when set, action type becomes `IVR` (new `ActionType::Ivr`).
- `RouteResult::Forward` continues to existing flow; `RouteResult::Ivr(IvrPlanCall)` (new variant) instructs `proxy_call::CallTask` to invoke IVR engine.
- Queue fallback: add `QueueFallbackAction::Ivr { plan_id, context }` so timeouts or failures can enter IVR.
- WebSocket/API: add `Command::IvrStatus { step_id, attempts, input }` for console to display.

## Observability & Metrics
- Emit structured logs per step: `{call_id, plan_id, step, event, detail}`.
- Counters: prompts played, invalid inputs, timeout exits, transfers per branch.
- Histogram: step latency, webhook latency.
- Optional span integration w/ OpenTelemetry (wrap each IVR session in span `ivr.plan_id`).

## Error Handling
- Missing prompt file -> fallback to TTS announcement and continue.
- Webhook failure -> configurable retry/backoff; default route to `on_error` step.
- Invalid plan reference -> log error, hang up with `503 Service Unavailable` or fall back to default trunk.
- Guard recursion depth for nested IVRs to prevent loops.

## Testing Strategy
1. **Unit Tests** (`src/call/tests/ivr_plan_test.rs`):
   - Prompt/input transitions, regex capture substitution, timeout behavior.
   - Action execution stubs verifying dial requests/webhook payloads.
2. **Integration Tests** (`tests/ivr_flow.rs`): simulate end-to-end call entering IVR, providing digits, ensuring final routing.
3. **Property Tests** (optional): random digit sequences vs. branch definitions ensure no panic.

## Rollout Plan
1. Implement data structures + parser.
2. Build IVR engine and hook to call loop (feature-flagged).
3. Add config loader & console UI.
4. Cover with tests, run `cargo test ivr_*`.
5. Document API + example.
6. Enable feature flag by default once stable.

## Example: “Welcome to our service—please dial an extension, or press 0 for the directory”
```toml
[plans.main_menu]
entry_step = "welcome"
calendars = {
   workday = {
      timezone = "Asia/Shanghai",
      weekly = [
         { days = ["mon", "tue", "wed", "thu", "fri"], start = "09:00", end = "18:00" }
      ],
      closed = [ { date = "2025-10-01" } ]
   }
}

[plans.main_menu.steps.welcome]
type = "prompt"
prompts = [ { tts = { text = "Welcome to our service—please dial an extension, or press 0 for the directory" } } ]
next = "collect"

[plans.main_menu.steps.collect]
type = "input"
min_digits = 1
max_digits = 4
regex = "^(0|[1-9][0-9]{2,3})$"
allow_barge_in = true
on_valid = { goto = "branch" }
on_timeout = { goto = "timeout" }
on_invalid = { goto = "invalid" }

[plans.main_menu.steps.branch]
type = "branch"
availability = { calendar = "workday", when_closed = { goto = "after_hours" } }
branches = [
  { match = "0", goto = "directory" },
  { regex = "^([1-9][0-9]{2,3})$", action = { transfer = { target = "sip:{1}@pbx.local" } } }
]
default = "invalid"

[plans.main_menu.steps.after_hours]
type = "prompt"
prompts = [ { tts = { text = "We're currently outside business hours. Please leave a message or call back on the next business day." } } ]
next = "after_hours_action"

[plans.main_menu.steps.after_hours_action]
type = "action"
action = { webhook = { url = "https://rustpbx.com/after_hours", payload = "{0}" } }
```
This replicates the previously discussed scenario: valid extensions go straight to SIP URI substitution, while `0` redirects to a directory workflow (another IVR or webhook).
