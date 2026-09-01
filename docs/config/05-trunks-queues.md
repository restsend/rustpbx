# Trunks & Queues

## Trunks (`[proxy.trunks]`)
Gateways to external SIP providers. Configured in `[proxy.trunks]` map or separate files.

```toml
[proxy.trunks.provider_a]
dest = "sip:sip.provider.com:5060"
# Optional failover
backup_dest = "sip:backup.provider.com"

# Authentication
username = "myuser"
password = "mypassword"

# Capacity
max_calls = 50
max_cps = 5          # Calls per second
weight = 10          # Relative weight for load balancing

# Traffic Control
direction = "outbound"       # inbound, outbound, bidirectional
inbound_hosts = ["203.0.113.50"] # Whitelist IPs
```

### Trunk Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `dest` | string | required | SIP URI of the gateway |
| `backup_dest` | string | none | Failover SIP URI |
| `username` / `password` | string | none | SIP authentication credentials |
| `codec` | [string] | `[]` | Allowed codecs (alias: `allow_codecs`, `audio_codecs`) |
| `transport` | string | none | Transport protocol override (e.g., `"tcp"`) |
| `max_calls` | int | none | Max concurrent calls |
| `max_cps` | int | none | Max calls per second |
| `weight` | int | none | Load balancing weight |
| `direction` | string | `"bidirectional"` | `inbound`, `outbound`, `bidirectional` |
| `inbound_hosts` | [string] | `[]` | Source IP whitelist for inbound calls |
| `disabled` | bool | `false` | Disable trunk without removing it |
| `country` | string | none | Country code for number normalization |
| `did_numbers` | [string] | `[]` | DID numbers owned by this trunk (inbound routing) |
| `call_id_mode` | string | none | Call-ID rewriting: `"prefix"`, `"suffix"`, `"none"` |
| `rewrite_hostport` | bool | `true` | Rewrite host:port in outgoing Contact headers |
| `recording` | table | none | Per-trunk recording policy override |
| `ringback` | table | none | Per-trunk ringback audio override |
| `max_ring_time` | int | none | Per-trunk max ring/setup time (seconds) before a no-answer call is rejected with 408. `0` disables the ring timeout for this trunk. Overrides the global `[proxy] max_ring_time` for calls routed through this trunk |
| `external_ip` | string | none | Override the IP advertised in SDP `c=`/`o=` lines and ICE candidates for this trunk's legs. Replaces the profile/global RTP external IP. Essential when some trunks terminate on an overlay network (Tailscale/WireGuard) that needs a different advertised IP than the public NAT address |
| `bind_ip` | string | none | Override the local IP RTP sockets bind to for this trunk's legs. Replaces the profile/global RTP bind IP |
| `profile` | string | none | Network profile id from `[[network_profile]]` in the main config (Console: trunk **Media Option → Network profile**). Applies grouped RTP/SDP and SIP Contact settings; per-trunk `external_ip` / `bind_ip` override the profile when set |
| `header_passthrough` | table | none | Control which custom headers from the original INVITE are forwarded to this trunk's outbound INVITE. `mode` is `"all"` (default), `"whitelist"`, or `"blacklist"`; `whitelist`/`blacklist` are header-name lists (case-insensitive). Standard SIP headers (`Via`/`From`/`To`/`Call-ID`/`CSeq`/`Contact`/...) are never forwarded. Unset (default) = forward nothing to external trunks; internal destinations (same realm / registered / home-proxy) always forward everything unless overridden by the route's `with_original_headers` |

### Custom Header Passthrough

Controls whether custom headers from the **original inbound INVITE** (e.g. `X-CRM-Ticket-Id`) are copied onto the outbound INVITE for the callee leg. Resolution order per callee target:

1. **Internal targets** (same realm, registered AOR, or home-proxy) → forward all custom headers.
2. **External trunk targets** → use the trunk's `header_passthrough` config; if unset, forward nothing.
3. **HTTP dynamic router** → the response's `with_original_headers` overrides the above (`true` = forward all, `false` = forward none). See [04-routing.md](04-routing.md).

This applies to every callee leg: direct dial, parallel fork, queue agent legs, transfers, and app-originated legs (targets resolved inside the session fall back to the per-target resolution above).

Example trunk configuration:

```toml
[proxy.trunks.provider_a]
header_passthrough = { mode = "all" }                # forward all custom headers
# header_passthrough = { mode = "whitelist", whitelist = ["X-Smart2Agent", "X-SmartParams"] }
# header_passthrough = { mode = "blacklist", blacklist = ["X-Token"] }
```

Standard SIP headers are never forwarded; only custom (non-standard) headers are subject to this rule.

### Trunk Registration

For trunks that require outbound registration:

```toml
[proxy.trunks.sip_provider]
dest = "sip:sip.provider.com:5060"
username = "myuser"
password = "mypassword"

# SIP registration (register at this trunk)
register_enabled = true
register_expires = 3600
# register_extra_headers = { "X-Client-ID" = "my-pbx" }
```

### Trunk Health Checks

Optional health monitoring for trunk availability:

```toml
[proxy.trunks.provider_a]
dest = "sip:sip.provider.com:5060"

health_check_enabled = true
health_check_interval_secs = 30   # Probe every 30s
health_check_probe_count = 3      # Fail after 3 failed probes
health_check_fallback_trunk = "backup-provider"  # Auto-failover
```

### Advanced Trunk Settings

```toml
[proxy.trunks.provider_a]
dest = "sip:sip.provider.com:5060"

# Call Admission Control
cac_policy = "loss_based"         # "loss_based" or "reject"
overflow_threshold = 90           # Trigger CAC at 90% capacity

# Media handling
media_mode = "auto"               # "auto", "none", "bypass", "force_transcode"
                                  # - auto: bridge only for app/queue flows
                                  # - none: no media proxy (SDP passthrough, RTP direct)
                                  # - bypass: SDP rewrite only, RTP direct
                                  # - force_transcode: always bridge through PBX
video_policy = "pass_through"      # "passthrough", "strip", "transcode"

# Per-trunk network profile (multi-path egress; see 01-platform.md)
profile = "overlay"               # [[network_profile]] id

# Per-trunk IP override (for overlay networks like Tailscale/WireGuard)
# These override the selected profile when set.
external_ip = "100.64.10.1"
bind_ip = "100.64.10.2"

# See [06-media-recording.md](06-media-recording.md) for the full media proxy
# reference, including latching, trunk-level vs server-level configuration,
# and recommended combinations for NAT / overlay scenarios.

# SIP header manipulation
header_rules = [
    { action = "add", name = "X-Client-ID", value = "rustpbx" },
    { action = "remove", name = "X-Internal-Info" },
]

# Forward original custom headers to this trunk's outgoing INVITE.
# Unset (default) -> forward nothing; internal destinations forward everything.
header_passthrough = { mode = "all" }            # all custom headers
# header_passthrough = { mode = "whitelist", whitelist = ["X-Smart2Agent", "X-SmartParams"] }
# header_passthrough = { mode = "blacklist", blacklist = ["X-Token"] }

# Number normalization
incoming_from_user_prefix = ""    # Strip prefix from inbound caller
incoming_to_user_prefix = ""      # Strip prefix from inbound callee
```

## Queues (`[proxy.queues]`)
Call distribution logic (ACD).

Calls dispatched to queue agents follow the same [custom header passthrough](#custom-header-passthrough) resolution: original INVITE custom headers are forwarded to internal agent legs, and to external agent trunks per their `header_passthrough` config.

```toml
[proxy.queues.support_main]
name = "General Support"
accept_immediately = true
passthrough_ringback = false
# acd_policy = "default"       # Reference to ACD policy (CC addon)

# Hold Music
[proxy.queues.support_main.hold]
audio_file = "sounds/hold_music.wav"
loop_playback = true

# Distribution Strategy
[proxy.queues.support_main.strategy]
mode = "sequential" # or "parallel" (ring-all)
wait_timeout_secs = 20

[[proxy.queues.support_main.strategy.targets]]
uri = "sip:1001@local"
label = "Alice"

[[proxy.queues.support_main.strategy.targets]]
uri = "sip:1002@local"
label = "Bob"

# Fallback (if no agents answer)
[proxy.queues.support_main.fallback]
redirect = "sip:voicemail@local" # or a queue URI, e.g. "queue:overflow?overflow_group=..." (embedded query params are honored)
# failure_code = 486
# failure_reason = "No agents available"

# Voice prompts (played to caller while waiting)
# [proxy.queues.support_main.voice_prompts]
# estimated_wait = "sounds/estimated_wait.wav"
# position = "sounds/position.wav"
# periodic = "sounds/thank_you.wav"
# periodic_interval_secs = 60
```

### Queue Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | none | Display name |
| `acd_policy` | string | none | ACD policy name (CC addon) |
| `accept_immediately` | bool | `false` | Call is accepted (200 OK) before agent answers |
| `passthrough_ringback` | bool | `false` | Forward callee ringback to caller |
| `hold` | table | none | Hold music config |
| `strategy` | table | required | Distribution strategy (mode, targets, timeout) |
| `fallback` | table | none | Fallback when no agent answers |
| `voice_prompts` | table | none | Voice announcements during wait |

### Queue Transfer Query Parameters

When transferring a call to a queue via `queue:<name>`, you can append query parameters to override queue configuration at runtime.

**`?return_app=<app>&return_target=<target>`** — Override the fallback action to transfer to an app (e.g. `ivr`, `voicemail`, `queue`, `conference`) instead of the configured fallback when no agents are available. Note: the older `return_ivr=` spelling is **not** parsed:

```
queue:support?return_app=ivr&return_target=main_menu
```

**`?target=<value>`** — Override the queue's configured agent targets with the given value. Supports `skillgroup:<id>` (resolved via AgentRegistry) or a SIP URI. Multiple `&target=` params are supported and dialed sequentially:

```
queue:support?target=skillgroup:sales                            # Single skill group
queue:support?target=sip:agent@pbx.com                           # Single SIP agent
queue:support?target=skillgroup:sales&target=skillgroup:support  # Multiple targets (sequential)
```

**`?overflow_group=<id>`** — 【Overflow override】Per-call overflow target skill groups. May be repeated, and comma-separated values are also accepted. When present, it **replaces** the whole escalation timeline for this call (no merging with the group's configured `overflow_groups`):

```
queue:support?overflow_group=support_l2                          # Single overflow group
queue:support?overflow_group=support_l2,support_l3               # Comma-separated list
queue:support?overflow_group=support_l2&overflow_group=support_l3
```

**`?overflow_after=<secs>`** — Overflow trigger threshold in seconds (overrides the escalation step thresholds). Without `overflow_group`, it rewrites the thresholds of the registry-synthesized plan.

**`?overflow_wait=<secs>`** — Queue max wait (`max_wait_secs`) in seconds; after this the caller is routed to the fallback / return app. This is separate from `overflow_after`: the former bounds the total queue wait, the latter schedules the overflow escalation.

**`?overflow_mode=<replace|cumulative>`** — Overflow mode: `replace` (re-dial into the new group) or `cumulative` (add the new group's agents alongside the current ones, fair round-robin). Defaults to `cumulative` when omitted.

**Priority:** URI params > ACD policy (`overflow.escalation_timeline`) > skill-group `overflow_groups` + `max_wait_secs`. Only fields present on the URI are overridden; the rest fall back to the registry-synthesized plan. Overflow params only take effect when the queue dials a skill-group target (`?target=skillgroup:...` or the queue's own skill-group strategy).

**Combined usage:**

```
queue:support?target=skillgroup:vip&overflow_group=support_l2&overflow_group=support_l3&overflow_after=30&overflow_mode=cumulative&overflow_wait=120&return_app=ivr&return_target=main_menu
```

### Queue Overflow Escalation (skill-group queues)

When a queue's dial target is a skill group (`skill-group:{id}`, via
`strategy.targets` or `?target=skillgroup:...`), the CC addon derives an
escalation plan for it: the call is scheduled on the **primary group only**
at first; after the configured queued-wait threshold the candidate set is
**widened** to the overflow groups and ordered fairly (round-robin, the
counter advances once per call) across the union — the new agents ring
alongside the primary ones (cumulative mode).

Two configuration sources, in order of precedence:

**1. Skill group `overflow_groups` + `max_wait_secs`** (simplest — no ACD
policy needed). Each overflow group becomes one escalation step at the
group's `max_wait_secs` threshold, cumulative + fair:

```toml
# skill_groups TOML (or the /api/cc/skill-groups API — same fields)
[[skill_groups]]
skill_group_id = "support"
skills_required = ["support"]
overflow_groups = ["support_l2", "support_l3"]  # widen targets
max_wait_secs = 30                              # widen threshold (per queue wait)
```

**2. ACD policy `overflow.escalation_timeline`** (explicit, takes
precedence when the group's `acd_policy` references a policy that defines
one):

```toml
[policies.p1.overflow]
mode = "cumulative"          # cumulative = ring alongside; replace = redial

[[policies.p1.overflow.escalation_timeline]]
threshold_secs = 20
skill_group_id = "support_l2"
fair = true                  # widen with round-robin ordering (default false)
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `threshold_secs` | int | required | Queued-wait seconds before this step triggers |
| `skill_group_id` | string | required | Group to widen to |
| `fair` | bool | `false` | Round-robin ordering across the widened union |
| `mode` | string | `"replace"` | `cumulative` (ring alongside) or `replace` (redial) |
