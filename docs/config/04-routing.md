# Routing

Routing determines how calls are processed. 

## Static Routes (`[[proxy.routes]]`)
Defined in TOML. Evaluated in order of `priority` (higher first), then definition order.

```toml
[[proxy.routes]]
name = "Internal Calls"
priority = 100
direction = "any" # inbound, outbound, any

# Match Conditions (Regex supported)
[proxy.routes.match]
"to.user" = "^10[0-9]{2}$"
# "from.host" = "internal.net"
# "header.X-My-Header" = "secret"

# Optional Rewrites
[proxy.routes.rewrite]
"to.host" = "127.0.0.1"

# Action
[proxy.routes.action]
type = "local" # or "forward", "reject", "queue"
```

### Forwarding Example
Forward specific prefix to a trunk.

```toml
[[proxy.routes]]
name = "Outbound US"
priority = 10

[proxy.routes.match]
"to.user" = "^1[2-9][0-9]{9}$"

[proxy.routes.action]
type = "forward"
dest = "provider-trunk" # Name of a defined trunk
```

### Queue Route
Send traffic to a call queue.

```toml
[[proxy.routes]]
name = "Support Line"

[proxy.routes.match]
"to.user" = "support"

[proxy.routes.action]
type = "queue"
queue = "support-queue" # Name of queue config
```

## HTTP Dynamic Router (`proxy.http_router`)
Ask an external service for routing instructions per call.

```toml
[proxy.http_router]
url = "http://route-engine/decision"
timeout_ms = 500
fallback_to_static = true # If HTTP fails, use static routes
```

**Request Payload:**
```json
{
  "call_id": "...",
  "from": "sip:alice@...",
  "to": "sip:bob@...",
  "source_addr": "1.2.3.4:5060"
}
```

**Response:**
```json
{
  "action": "forward",
  "targets": ["sip:1001@10.0.0.2"],
  "record": true
}
```
