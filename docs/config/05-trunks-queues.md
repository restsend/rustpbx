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
media_mode = "auto"               # "auto", "all", "none", "nat", "bypass"
video_policy = "passthrough"      # "passthrough", "strip", "transcode"

# SIP header manipulation
header_rules = [
    { action = "add", name = "X-Client-ID", value = "rustpbx" },
    { action = "remove", name = "X-Internal-Info" },
]

# Number normalization
incoming_from_user_prefix = ""    # Strip prefix from inbound caller
incoming_to_user_prefix = ""      # Strip prefix from inbound callee
```

## Queues (`[proxy.queues]`)
Call distribution logic (ACD).

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
action = "redirect" # or "hangup", "queue"
redirect = "sip:voicemail@local"
# queue_ref = "overflow_queue"

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

**`?return_ivr=<name>`** — Override the fallback action to transfer to an IVR instead of the configured fallback when no agents are available:

```
queue:support?return_ivr=main_menu
```

**`?target=<value>`** — Override the queue's configured agent targets with the given value. Supports `skillgroup:<id>` (resolved via AgentRegistry) or a SIP URI. Multiple `&target=` params are supported and dialed sequentially:

```
queue:support?target=skillgroup:sales                            # Single skill group
queue:support?target=sip:agent@pbx.com                           # Single SIP agent
queue:support?target=skillgroup:sales&target=skillgroup:support  # Multiple targets (sequential)
```

**Combined usage:**

```
queue:support?target=skillgroup:sales&return_ivr=main_menu
```
