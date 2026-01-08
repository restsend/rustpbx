# Authentication & Users

RustPBX supports multiple backend types for user authentication and retrieval. You can chain multiple backends in `proxy.user_backends`. 

The backends are queried in the order they are defined. If a user is not found in the first backend, the next one is checked.

> **Management**: You can add, remove, and configure these backends in the **Web Console** under **Settings > Proxy Settings**. The console also provides a **Test** feature to verify backend connectivity and response logic before applying changes.

## 1. Memory Backend (Static)
Best for small, static deployments or testing.

```toml
[[proxy.user_backends]]
type = "memory"

[[proxy.user_backends.users]]
username = "1001"
password = "secret-password"
realm = "example.com"  # Optional
display_name = "Alice"
enabled = true
# Allow calls from this user without Registration (IP-based auth usually handled elsewhere)
allow_guest_calls = false 
```

## 2. Database Backend
Loads users from the SQL database (`database_url`).

```toml
[[proxy.user_backends]]
type = "database"
# Optional overrides for table schema
table_name = "users"
id_column = "id"
username_column = "username"
password_column = "password"
realm_column = "realm"
enabled_column = "is_active"
```

## 3. HTTP Backend (Remote)
Offloads authentication to an external web service.

```toml
[[proxy.user_backends]]
type = "http"
url = "http://auth-service/verify"
method = "POST"           # Optional: "GET" (default) or "POST"
# username_field = "user"   # Optional: default "username"
# realm_field = "domain"    # Optional: default "realm"
# headers = { "X-Api-Key" = "secret" }
```

### Protocol Details
- **Request (GET)**: `?username=1001&realm=example.com`
- **Request (POST)**: Form-encoded body with `username` and `realm`.
- **Response (Success)**: Must return a HTTP 200 OK with a JSON object representing the `SipUser`.
- **Response (Error)**: HTTP 4xx/5xx with a JSON error payload.

#### Success Payload (`SipUser`)
| Field | Type | Description |
|-------|------|-------------|
| `username` | string | SIP username |
| `password` | string | SIP password |
| `realm` | string | SIP realm (optional) |
| `display_name` | string | Caller ID name (optional) |
| `enabled` | bool | Whether the user is active |
| `allow_guest_calls` | bool | Allow calls without registration |

#### Error Payload
If the authentication fails, the server should return a non-2xx status code with the following JSON:

```json
{
  "reason": "invalid_credentials",
  "message": "Optional detailed message"
}
```

**Known Reasons:**
- `not_found`, `not_user`: User doesn't exist.
- `invalid_password`, `invalid_credentials`: Basic auth failure.
- `disabled`, `blocked`: Account is locked or disabled.
- `spam`, `spam_detected`: Account flagged for spamming.
- `payment_required`: Insufficient balance.

### Python Example (FastAPI)
```python
from fastapi import FastAPI, Form, HTTPException
from typing import Optional, List

app = FastAPI()

@app.post("/verify")
def verify_user(username: str = Form(...), realm: str = Form(...)):
    # Database lookup or logic here
    if username == "1001":
        return {
            "id": 1,
            "enabled": True,
            "username": "1001",
            "password": "secret-password",
            "realm": realm,
            "display_name": "Alice Cooper",
            "allow_guest_calls": False
        }
    
    raise HTTPException(status_code=404, detail="User not found")
```

## 4. Plain Text Backend
Loads users from a simple text file.

```toml
[[proxy.user_backends]]
type = "plain"
path = "./users.txt"
```

## 5. Extension Backend
Used for short-lived, dynamic extensions (often internal).

```toml
[[proxy.user_backends]]
type = "extension"
ttl = 3600 # Cache time in seconds
```

---

## Locator (Registrar Storage)
Configures where "Where is User X?" data is stored.

### Memory (Default)
Fast, but lost on restart.
```toml
[proxy.locator]
type = "memory"
```

### Database
Persistent registrations.
```toml
[proxy.locator]
type = "database"
url = "sqlite://rustpbx.sqlite3" # Can share main DB
```

### HTTP (Remote)
Query external registry.
```toml
[proxy.locator]
type = "http"
url = "http://registry-service/lookup"
```

## Locator Webhook
Trigger notifications when user registration status changes.

You can configure and test this webhook directly in the **Web Console** (**Settings > Proxy Settings**). The "Test webhook" feature sends a mock registration event to your endpoint to verify connectivity and custom headers.

```toml
[proxy.locator_webhook]
url = "http://your-app/sip-events"
events = ["registered", "unregistered", "offline"]
timeout_ms = 5000
headers = { "X-API-Key" = "my-secret-key", "Authorization" = "Bearer token123" }
```

### Event Payload
The webhook sends a POST request with a JSON body:

```json
{
  "event": "registered",
  "location": {
    "aor": "sip:1001@example.com",
    "expires": 3600,
    "destination": "udp:192.168.1.100:5060",
    "supports_webrtc": false,
    "transport": "Udp",
    "user_agent": "Zoiper 5"
  },
  "timestamp": 1704537600
}
```

- `event`: "registered", "unregistered", or "offline".
- `location`: Sip location details (only for `registered` and `unregistered`).
- `locations`: Array of locations (only for `offline`).
- `timestamp`: Unix timestamp in seconds.
