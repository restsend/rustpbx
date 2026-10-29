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
| `ringback` | table | none | Per-trunk ringback audio override — see [Ringback & failure tones](#ringback--failure-tones) |
| `max_ring_time` | int | none | Per-trunk max ring/setup time (seconds) before a no-answer call is rejected with 408. `0` disables the ring timeout for this trunk. Overrides the global `[proxy] max_ring_time` for calls routed through this trunk |
| `external_ip` | string | none | Override the IP advertised in SDP `c=`/`o=` lines and ICE candidates for this trunk's legs. Replaces the profile/global RTP external IP. Essential when some trunks terminate on an overlay network (Tailscale/WireGuard) that needs a different advertised IP than the public NAT address |
| `bind_ip` | string | none | Override the local IP RTP sockets bind to for this trunk's legs. Replaces the profile/global RTP bind IP |
| `nat_fix` | bool | none | Override the global `[proxy] nat_fix` for SIP messages received from this trunk (source IP matched against `inbound_hosts`, `dest` and `backup_dest`; IP/CIDR entries only, hostnames are not resolved). `true` rewrites a private-IP Contact to the observed source address, `false` keeps it as sent. Unset = follow the global setting (Console: **General → SIP NAT fix**, stored as `metadata.sbc.nat_fix`) |
| `profile` | string | none | Network profile id from `[[network_profile]]` in the main config (Console: trunk **Media Option → Network profile**). Applies grouped RTP/SDP and SIP Contact settings; per-trunk `external_ip` / `bind_ip` override the profile when set |
| `header_passthrough` | table | none | Control which custom headers (unparsed `X-`/`P-` headers such as `X-Smart2Agent` or `P-Early-Media`) are copied between the call legs when calls go to this trunk, in both directions: the original INVITE's onto this trunk's INVITE, and this trunk's 18x/200 back onto the caller's responses. `mode` is `"all"` (default), `"whitelist"`, `"blacklist"`, or `"x_only"`; `whitelist`/`blacklist` are header-name lists (case-insensitive, a trailing `*` matches by prefix). Standard SIP headers are never copied. Unset (default) = copy nothing for external trunks; internal destinations (same realm / registered / home-proxy) always copy all custom headers unless overridden by the route's `with_original_headers` |

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
# header_passthrough = { mode = "x_only" }           # only X- prefixed headers
```

> Desk dial-out metadata (per-key `X-<key>` headers: `X-taskid` / `X-aid` /
> `X-callright` / …) reaches a CCF/carrier-facing trunk by configuring that
> trunk with `header_passthrough = { mode = "x_only" }` (X- headers only) or
> `mode = "all"`. When a trunk has no rule and the original INVITE carries
> custom headers, the dropped header names are logged (warn) so field loss
> is never silent.

Standard SIP headers (`Require`, `Identity`, `P-Asserted-Identity`, …) are never copied; only custom headers (unparsed `X-`/`P-` headers such as `X-Smart2Agent` or `P-Early-Media`) are subject to this rule, in both directions.

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

### Ringback & Failure Tones

Two configuration layers control the audio callers hear during setup and
failure. Each value is an audio file path (relative paths resolve against
`config/`, e.g. `sounds/x.wav` → `config/sounds/x.wav`), an `http(s)://` URL,
or a `tone://frequency,duration_ms` spec rendered into a WAV.

Fields: `ring` (early-media ringback while the callee rings), `busy` (486),
`reject` (603), `offline` (480), `notfound` (404), `noanswer` (408/487),
`error` (5xx). Failure audio plays ONCE to natural completion before the
rejection is sent; `tone://` specs play for the given duration (1 s minimum).

- **Global** `[proxy.audio_profile]` applies to every call. Built-in defaults
  are `tone://` beeps for busy/reject/offline/notfound/noanswer and the English
  `sounds/service_unavailable_en.mp3` for `error`. A Chinese voice set ships
  ready-made (`sounds/failure-busy-zh.wav`, `failure-offline-zh.wav`,
  `failure-notfound-zh.wav`, `failure-noanswer-zh.wav`,
  `failure-service-zh.wav` — regenerate with
  `python3 scripts/generate_failure_sounds.py`). Declaring an empty table
  disables all built-in failure audio.
- **Per-trunk** `[proxy.trunks.<name>.ringback]` overrides individual fields
  on top of the global profile (unset fields inherit).

```toml
[proxy.audio_profile]
busy = "sounds/failure-busy-zh.wav"
noanswer = "sounds/failure-noanswer-zh.wav"

[proxy.trunks.wuhoo.ringback]
ring = "/sounds/company_ringback.wav"
busy = "tone://480,3000"
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

## DIDWW static inbound and outbound voice

Use the standalone [DIDWW example](../../config/trunks/didww.toml.example)
to route one DID to a registered extension and let that authenticated extension
call one approved destination through a DIDWW outbound trunk. It uses the
community SIP core, UDP signaling, G.711 PCMU/PCMA and anchored RTP. No carrier
SDK or Rust source change is required by this configuration.

The example is a complete **main config**, not a `trunks_files` include. Its
`.example` suffix prevents the normal `config/trunks/*.toml` glob from loading
it accidentally. Do not rename it to `.toml` in that include directory.

### Prepare the endpoints

You need a RustPBX installation with its runtime assets, a registered SIP
softphone or other media-capable user agent, and a DIDWW account with an active
DID and outbound trunk access. Start with a single approved test destination;
calls can incur provider charges. Keep credentials and completed configuration
files outside version control.

1. In DIDWW, create an **inbound SIP trunk with Static Endpoint** delivery to
   your reachable RustPBX SIP address and UDP port. Assign the intended DID to
   that trunk. This example recognizes the carrier by source IP; leave the
   optional inbound SIP authorization disabled. It does not register with
   DIDWW. See [Create a SIP trunk](https://doc.didww.com/voice/inbound-trunks/sip-trunks/how-to-guides/create-sip-trunk.html).
2. Create/enable a DIDWW outbound trunk using credentials and the appropriate
   source-IP restrictions. Privately obtain its username/password, choose a
   current regional SIP endpoint, allow your actual signaling/media addresses,
   and permit the intended Caller ID and destination. Inbound and outbound
   trunks have separate purposes and settings. See [Outbound trunks](https://doc.didww.com/voice/outbound-trunks/index.html).
3. Choose the local numeric extension and a strong extension password. Register
   the softphone to the configured RustPBX realm and SIP port using that
   extension. Allow its trusted network separately from the carrier ranges.
   Never classify the extension's network as `inbound_hosts` for a carrier.

### Fill the standalone configuration

Copy `config/trunks/didww.toml.example` to a private main file such as
`didww.toml`, replace **every** `<...>` placeholder, then start RustPBX with
`rustpbx --conf didww.toml` from a directory containing the normal runtime
assets. Restart after changing the main configuration. The sample does not
read generated trunk/route include files; do not also create duplicate console
trunks or routes with the same names.

| Placeholder | Value |
|---|---|
| `PBX_BIND_IP` | Local interface address on which RustPBX listens |
| `PBX_SIP_IP`, `PBX_MEDIA_IP` | Addresses reachable by the SIP peers and RTP peers; configure signaling Contact and media SDP separately |
| `PBX_REALM` | The realm used by the extension's SIP account; match its registration settings |
| `CONSOLE_SESSION_SECRET` | A long random private session secret |
| `DIDWW_SIGNALING_IP`, `DIDWW_SIGNALING_CIDR` | Current carrier source address/range for inbound recognition; repeat `inbound_hosts` and `acl_rules` entries for all required ranges |
| `EXTENSION_NETWORK_CIDR` | Only the trusted extension network |
| `LOCAL_EXTENSION`, `EXTENSION_SECRET` | Numeric extension and its private SIP password |
| `DIDWW_OUTBOUND_HOST` | Selected endpoint hostname, without `sip:` or the port |
| `DIDWW_USERNAME`, `DIDWW_PASSWORD` | Private outbound trunk credentials |
| `INBOUND_DID_DIGITS` | Country code and DID digits as delivered in `To`; the route allows an optional leading `+` |
| `AUTHORIZED_CALLER_ID_DIGITS` | Permitted outbound Caller ID in international digits |
| `ALLOWED_DESTINATION_DIGITS` | One authorized test destination in international digits |

The outbound number is intentionally an exact match. Expand it only after
testing and applying an explicit dialing policy. The inbound DID route has
higher priority than the carrier catch/reject route, which prevents unmatched
carrier traffic from falling through to outbound service. Other unmatched
calls receive 403. Route action fields (`action`, `dest`, `select`) belong on
`[[proxy.routes]]`, before its `[proxy.routes.match]` table.

The `acl`, `auth`, `registrar`, `call` module order is intentional. Source-IP
recognition admits provider traffic as a trunk; local extensions still use SIP
digest authentication. Preserve both carrier recognition and extension
authentication. The sample constrains concurrent calls and outbound CPS to one.

### Signaling and audio reachability

Allow the current DIDWW signaling and RTP networks through the firewall, plus
the intended extension path. Do not allow all sources to an exposed SIP port.
The example uses UDP 5060 and a small local RTP range, 20000–20099; forward the
chosen local ports correctly through NAT and match the SDP/Contact addresses
to the real deployment. Carrier RTP ports are distinct from the local RustPBX
RTP range. Containers require reachable advertised addresses and UDP mappings
or an appropriate shared network; opening only HTTP is insufficient.

Keep PCMU/PCMA enabled on the softphone and carrier trunks. The endpoint must
actively transmit RTP after answer; do not rely on both sides waiting for
received audio before transmitting. A successful OPTIONS reply confirms SIP
reachability only. See [SIP connectivity](https://doc.didww.com/voice/inbound-trunks/sip-trunks/sip-connectivity.html),
[Outbound SIP information](https://doc.didww.com/voice/outbound-trunks/outbound-sip-information.html)
and [Platform & Networking](01-platform.md).

An outbound `401 Unauthorized` followed by a credentialed INVITE is a normal
digest exchange. DIDWW's advertised digest realm is `out.didww.com`, including
when a regional hostname is selected. A final 401/403 requires checking the
credentials, observed source IP, permitted Caller ID and destination policy;
do not keep redialing blindly. See [Authentication and security](https://doc.didww.com/voice/outbound-trunks/authentication-security.html).

### Verify in both directions

First validate TOML parsing and confirm that RustPBX loads exactly the intended
two trunks and four routes. The local AMI health endpoint is
`/ami/v1/health` (subject to the AMI allowlist), not `/health`.
Check registration and matched route/trunk in your console or private logs.

Use an isolated SIP test server to test the configuration before making real
calls. Substitute only the test server's addresses, synthetic numbers and test
credentials in a private copy, retain the same route restrictions, and keep
the test isolated from provider networks. Test inbound delivery to the
extension and outbound authenticated delivery to the server separately. A
server-generated tone should reach the softphone, and the softphone should
transmit known audio back. Check decoded audio and per-call RTP in both
directions; receiving a SIP answer alone is insufficient. Record these as
simulated checks, not carrier interoperability results.

For live checks, use the DID assigned to the inbound trunk and the outbound
source/Caller ID permitted by DIDWW. The person testing supplies a calling
number for inbound and a receiving number for outbound; these may differ.
Confirm the requested direction is ready before each call.

| Direction | Procedure | Success |
|---|---|---|
| Inbound | Call the assigned DID from the agreed calling number; answer at the intended registered extension | Correct extension rings, caller identity matches, both parties exchange intelligible speech, hangup closes the dialog |
| Outbound | Dial the one approved receiving number from the authenticated extension; answer on that phone | Correct outbound trunk/authentication, permitted caller identity, intelligible speech both ways, clean hangup |

Collect call-specific SIP answer/ACK/BYE, route selection and RTP evidence,
with at least 50 RTP packets each way as a basic media-flow check. Packet
counts complement audible confirmation; they do not prove speech quality.
Keep real numbers, credentials and raw traces private. Record the exact
RustPBX revision and configuration tested, and clearly distinguish simulated
checks from actual provider calls. A configuration that merely parses, starts,
or registers has **not** passed live interoperability verification.

After testing, stop test calls, revoke temporary credentials/permissions,
restore the intended dialing restrictions and remove private test copies.
This minimal setup does not establish emergency calling, registration-based
inbound delivery, TLS/SRTP, failover or production-capacity behavior; assess
and test those separately.

## Queues (`[proxy.queues]`)
Call distribution logic (ACD).

Calls dispatched to queue agents follow the same [custom header passthrough](#custom-header-passthrough) resolution: original INVITE custom headers are forwarded to internal agent legs, and to external agent trunks per their `header_passthrough` config.

```toml
[proxy.queues.support_main]
name = "General Support"
accept_immediately = true
passthrough_ringback = false
# acd_policy = "default"       # Reference to ACD policy (CC addon)

# Hold Music (per-queue override)
[proxy.queues.support_main.hold]
audio_file = "sounds/hold_music.wav"
loop_playback = true

# PBX-wide queue wait music fallback (`[proxy] queue_hold_music`, top-level
# `[proxy]` section in config.toml): applies when the queue itself has no
# hold config; per-queue config wins. Without it, skill-group queues fall
# back to the built-in ringback and route queues stay silent (historical
# behavior). `voicemail_greeting` works the same way for voicemail greetings.

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
