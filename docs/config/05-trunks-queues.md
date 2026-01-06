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
max_cps = 5 # Calls per second

# Traffic Control
direction = "outbound" # inbound, outbound, bidirectional
inbound_hosts = ["203.0.113.50", "203.0.113.51"] # Whitelist IPs
```

## Queues (`[proxy.queues]`)
Call distribution logic (ACD).

```toml
[proxy.queues.support_main]
name = "General Support"
accept_immediately = true
passthrough_ringback = false

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
```
