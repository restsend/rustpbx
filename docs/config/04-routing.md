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
| `timeout`| int | Call timeout in seconds |
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
