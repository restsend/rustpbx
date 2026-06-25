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

# Action fields are flattened into the route:
# action = "forward"
# dest = "trunk-name"
# select = "sequential"
```

### Route Rule Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | required | Route name |
| `description` | string | `""` | Optional description |
| `priority` | int | `0` | Evaluation order (higher = first) |
| `source_trunks` | [string] | `[]` | Match only if call arrived via these trunk names |
| `source_trunk_ids` | [int] | `[]` | Match only if call arrived via these trunk IDs |
| `match` | table | required | Match conditions (regex on SIP fields) |
| `rewrite` | table | none | Optional SIP header/user/host rewrites |
| `codecs` | [string] | `[]` | Restrict allowed codecs for this route |
| `disable_ice_servers` | bool | `false` | Disable ICE for this route |
| `policy` | string | none | Billing policy reference |
| `disabled` | bool | `false` | Disable route without removing it |
| *flattened action fields* | — | — | See RouteAction below |

### RouteAction Fields

Flattened into the route (not nested under `[proxy.routes.action]`).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `action` | string | `""` | `forward`, `reject`, `queue`, `app`, or omitted for local |
| `dest` | string | none | Target trunk or SIP URI (for `forward`) |
| `select` | string | `"rr"` | Target selection: `"rr"` (round-robin), `"sequential"`, `"parallel"` |
| `hash_key` | string | none | Consistent hashing key for destination selection |
| `reject` | int | `403` | SIP status code for `reject` action |
| `queue` | string | none | Queue name for `queue` action |
| `app` | string | none | Application name (e.g., `"ivr"`) |
| `app_params` | table | none | Application-specific parameters (JSON) |
| `auto_answer` | bool | `true` | Auto-answer the call before routing to app |

### Forwarding Example
Forward specific prefix to a trunk.

```toml
[[proxy.routes]]
name = "Outbound US"
priority = 10

[proxy.routes.match]
"to.user" = "^1[2-9][0-9]{9}$"

# Route action fields are flattened:
action = "forward"
dest = "provider-trunk" # Name of a defined trunk
select = "sequential"
```

### Queue Route
Send traffic to a call queue.

```toml
[[proxy.routes]]
name = "Support Line"

[proxy.routes.match]
"to.user" = "support"

# Route action fields are flattened:
action = "queue"
queue = "support-queue" # Name of queue config
```

## HTTP Dynamic Router (`proxy.http_router`)
Ask an external service for routing instructions per call. 

Management and testing of the HTTP Router are available in the **Web Console** (**Settings > Proxy Settings**). The "Test router" button sends a sample INVITE payload to your service to ensure it responds with a valid routing JSON.

```toml
[proxy.http_router]
url = "http://route-engine/decision"
timeout_ms = 500
fallback_to_static = true # If HTTP fails, use static routes
headers = { "X-Api-Key" = "secret" }
```

### Protocol Details

**Request Payload (JSON POST):**
```json
{
  "call_id": "...",
  "from": "sip:alice@...",
  "to": "sip:bob@...",
  "source_addr": "1.2.3.4:5060",
  "direction": "internal",
  "method": "INVITE",
  "uri": "sip:1001@example.com",
  "headers": {
    "User-Agent": "Linphone/...",
    "X-Custom-Info": "..."
  },
  "body": ""
}
```

**Response Payload (JSON):**
| Field | Type | Description |
|-------|------|-------------|
| `action` | string | `forward`, `reject`, `abort`, `not_handled`, `spam` |
| `targets` | [string] | List of SIP URIs (required for `forward`) |
| `strategy`| string | `sequential` or `parallel` (default `sequential`) |
| `status` | int | SIP Status Code (for `reject`, `abort`, default `403`) |
| `reason` | string | Reason phrase (for `reject`, `abort`, `spam`) |
| `record` | bool | Whether to record this call |
| `timeout`| int | Maximum call duration in seconds (default: 3600) |
| `max_ring_time`| int | Max ring time in seconds for call setup/ringback phase (default: 60, clamped: 30-120) |
| `rtp_timeout` | int | RTP timeout per direction in seconds — if no audio received on either direction for this duration, call is terminated. Overrides proxy-level `rtp_timeout`. Set to `0` to disable. (default: uses proxy config, 30s) |
| `media_proxy` | string | Media proxy mode: `auto`, `all`, `none`, `nat` |
| `headers` | object | Custom SIP headers to add to the outgoing INVITE (key-value) |
| `with_original_headers` | bool | Whether to forward original headers (except core SIP headers) |
| `extensions` | object | Custom key-value pairs stored in the dialplan (useful for CDRs) |

### Python Example (Flask)
```python
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route("/decision", methods=["POST"])
def route_call():
    data = request.json
    call_to = data.get("to", "")
    
    # Custom business logic: route 1xxx to internal extensions
    if "sip:1" in call_to:
        return jsonify({
            "action": "forward",
            "targets": [call_to.replace("sip:", "sip:ext_")],
            "strategy": "sequential",
            "record": True,
            "media_proxy": "all",  # Force media proxy
            "headers": {
                "X-Custom-Info": "Routed-By-Python"
            },
            "extensions": {
                "account_id": "ACC123",
                "customer_tier": "gold"
            }
        })
    
    # Reject everything else
    return jsonify({
        "action": "reject",
        "status": 403,
        "reason": "Forbidden"
    })

if __name__ == "__main__":
    app.run(port=5000)
```
