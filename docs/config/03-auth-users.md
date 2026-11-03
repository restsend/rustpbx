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
# Optional: separate database for extensions (defaults to main database_url)
# database_url = "sqlite://extensions.sqlite3"
ttl = 3600 # Cache time in seconds
```

---

## HTTP Token Auth Backend (One-Shot Registration)

The HTTP Backend can be configured for **one-shot token authentication** — SIP clients carrying a token in a configurable header (e.g. `X-Auth-Token`) are authenticated immediately by the external HTTP service, **without the standard Digest 401/407 challenge round-trip**.

This is similar to JWT Fast Registration, but delegates token validation to an **external HTTP service** instead of validating locally. The same HTTP endpoint serves both paths:

```
REGISTER with X-Auth-Token header
      │
      ▼
┌─ AuthBackend chain (before 401) ──────────────────────┐
│ HttpTokenAuthBackend:                                 │
│   1. Check SIP request for token_header               │
│   2. Token present → call HTTP service with token     │
│      ├ 200 + SipUser → authenticated (no 407)         │
│      └ non-200 → fall through to Digest               │
│   3. No token → fall through to Digest                │
└───────────────────────────────────────────────────────┘
      │ (token invalid or absent)
      ▼
┌─ Digest 401/407 flow (existing) ──────────────────────┐
│ UserBackend::get_user() → same HTTP URL               │
│ External service returns SipUser with password        │
│ → Digest verification → registration success          │
└───────────────────────────────────────────────────────┘
```

### Configuration

Add `token_header` to any HTTP user backend to enable one-shot token auth:

```toml
[[proxy.user_backends]]
type = "http"
url = "https://auth-service.example.com/sip-auth"
method = "POST"
sip_headers = ["X-Auth-Token"]     # Forward token to external service
token_header = "X-Auth-Token"      # Enable one-shot token auth

# Optional: HTTP client tuning
# http_timeout_ms = 5000            # Default: 5000
# http_retry_count = 1             # Default: 1 (retries on network error + 5xx)
# http_retry_delay_ms = 500        # Default: 500

# Optional: Token cache (avoids repeated HTTP calls for the same token)
# token_cache_ttl_secs = 60        # Default: 0 (disabled). Recommended: 30-300
# token_cache_size = 10000         # Default: 10000 (LRU eviction, prevents memory leak)
```

### Configuration Fields

| Field | Default | Description |
|-------|---------|-------------|
| `token_header` | *(none)* | SIP header name that carries the auth token. When set, enables one-shot token auth. |
| `http_timeout_ms` | `5000` | HTTP request timeout in milliseconds. |
| `http_retry_count` | `1` | Number of retries on network errors and 5xx responses. |
| `http_retry_delay_ms` | `500` | Delay between retry attempts. |
| `token_cache_ttl_secs` | `0` | Token cache TTL in seconds. `0` disables caching. Recommended: 30–300. |
| `token_cache_size` | `10000` | Maximum cached token entries. Uses LRU eviction to prevent unbounded memory growth. |

### Token Cache & Memory Safety

When `token_cache_ttl_secs > 0`, successful token validations are cached:

- **Key**: SHA-256 hash of the token (not stored in plaintext)
- **Value**: The authenticated `SipUser` + insertion timestamp
- **Eviction**: LRU (Least Recently Used) — when the cache is full, the oldest entry is evicted
- **Expiry**: Entries expire after `token_cache_ttl_secs` and are lazily removed on next access

This prevents memory leaks: the cache is bounded by `token_cache_size` and entries auto-expire by TTL.

### External Service Protocol

The external HTTP service receives **the same request format** for both token auth and Digest password lookup. The presence of the token field distinguishes the two:

**Token Auth Request (POST)**:
```
POST /sip-auth
Content-Type: application/x-www-form-urlencoded

username=1001&realm=example.com&request_uri=sip:example.com&X-Auth-Token=abc123
```

**Digest Password Lookup (POST)** — same endpoint, no token:
```
POST /sip-auth
Content-Type: application/x-www-form-urlencoded

username=1001&realm=example.com&request_uri=sip:example.com
```

The external service can implement smart logic based on whether the token is present.

**Success Response**: HTTP 200 + SipUser JSON (same format as existing HTTP backend)

**Error Response**: HTTP 4xx/5xx + JSON error payload (same format as existing HTTP backend)

### External Service Example (FastAPI)

```python
from fastapi import FastAPI, Form, HTTPException

app = FastAPI()

VALID_TOKENS = {
    "abc123": {"username": "1001", "display_name": "Alice"},
    "def456": {"username": "1002", "display_name": "Bob"},
}

@app.post("/sip-auth")
def sip_auth(
    username: str = Form(...),
    realm: str = Form(...),
    request_uri: str = Form(""),
    # Token forwarded via sip_headers config; absent during Digest password lookup
    token: str = Form(None, alias="X-Auth-Token"),
):
    if token:
        # One-shot token auth (no 407)
        token_data = VALID_TOKENS.get(token)
        if token_data and token_data["username"] == username:
            return {
                "id": 1,
                "enabled": True,
                "username": username,
                "realm": realm,
                "display_name": token_data["display_name"],
                # No password needed — token is already validated
            }
        raise HTTPException(403, {"reason": "invalid_credentials", "message": "bad token"})
    else:
        # Digest password lookup (after 401 challenge)
        return {
            "id": 1,
            "enabled": True,
            "username": username,
            "realm": realm,
            "password": "secret-hash",  # Needed for Digest verification
        }
```

### Auth Backend Chain Order

When both JWT and HTTP token auth are configured:

1. **JwtAuthBackend** — local JWT validation (fastest)
2. **HttpTokenAuthBackend** — remote HTTP token validation
3. Other custom AuthBackends
4. WS pre-auth registry (WebSocket connections)
5. Digest 401/407 fallback

---

## JWT Fast Registration (No 401/407 Challenge)

RustPBX supports JWT (HS256) based **fast registration** — SIP clients carrying a valid JWT in the REGISTER request are authenticated immediately, without the standard Digest 401/407 challenge round-trip.

This is useful for WebRTC softphones (e.g. cc-phone SDK) that obtain a JWT from an external auth service and want to register in a single round-trip.

### How It Works

```
External Auth Service issues JWT → SDK connects and registers with JWT
  ↓
AuthModule checks auth_backend chain:
  1. JwtAuthBackend (validates JWT signature + exp + claims)
  2. ... other backends ...
  3. Digest 401/407 fallback (if no JWT or JWT invalid)
```

If the JWT is valid, the request passes through with **zero challenge round-trips**. If the JWT is absent or invalid, the request falls back to standard Digest authentication. **Existing clients are unaffected.**

### Two Authentication Paths

| Path | Mechanism | Transport | Use Case |
|------|-----------|-----------|----------|
| **Path A** (SIP header) | `X-Auth-Token: <jwt>` in REGISTER | All (UDP/TCP/TLS/WS) | Universal, transport-agnostic |
| **Path B** (WS pre-auth) | `?token=<jwt>` or `Authorization: Bearer <jwt>` at WebSocket upgrade | WS/WSS only | WebRTC clients, eliminates even the SIP-level header |

Both paths can be used simultaneously. Path B is automatically enabled when JWT auth is configured and the client connects via WebSocket.

### Configuration

```toml
[proxy.jwt_auth]
enabled = true
secret = "your-shared-hs256-secret"    # HS256 signing secret (shared with JWT issuer)
user_id_claim = "userId"               # JWT claim that maps to SIP username/extension
# issuer = "my-platform"               # Optional: validate iss claim
# audience = "rustpbx-sip"             # Optional: validate aud claim
# sip_header_name = "X-Auth-Token"     # Default: "X-Auth-Token" (path A)
# ws_token_param = "token"             # Default: "token" (path B query param name)
# check_local_user = false             # Default: false. If true, look up user in user_backend chain
```

### Configuration Fields

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable/disable JWT auth backend |
| `secret` | *(required)* | HS256 shared secret. Must match the JWT issuer's signing key. |
| `user_id_claim` | `"userId"` | JWT claim name whose value maps to the SIP username. Supports string and numeric values. |
| `issuer` | *(none)* | Expected `iss` claim value. If set, JWTs with mismatched `iss` are rejected. |
| `audience` | *(none)* | Expected `aud` claim value. |
| `sip_header_name` | `"X-Auth-Token"` | SIP header name carrying the JWT in path A. |
| `ws_token_param` | `"token"` | Query parameter name for JWT in WebSocket URL (path B). Also checks `Authorization: Bearer` header. |
| `check_local_user` | `false` | If `true`, look up the user in the `user_backend` chain after JWT validation (checks `enabled`, loads `display_name`, call forwarding, etc.). If `false`, creates a minimal `SipUser` from JWT claims only. |

### check_local_user: true vs false

| | `false` (default) | `true` |
|---|---|---|
| HTTP backend needed | No | No (uses local DB/cache) |
| User existence check | None | Verifies user exists in user_backend |
| `enabled` check | JWT valid = user enabled | Checks `login_disabled` in DB |
| `display_name` | From JWT `name` claim | From database |
| Call forwarding / voicemail | Not available | Available from DB |
| Performance | Fastest (zero I/O) | One local query (with LRU cache) |

**Recommendation**: Use `check_local_user = true` in production to ensure disabled extensions cannot register even with a valid JWT.

### JWT Format

The JWT must use **HS256** algorithm. Required/optional claims:

| Claim | Required | Description |
|-------|----------|-------------|
| `<user_id_claim>` | Yes | Maps to SIP username (e.g. `"userId": "1001"`) |
| `exp` | Recommended | Expiration timestamp (Unix seconds). If absent, token never expires. |
| `iss` | If configured | Issuer. Must match `issuer` config. |
| `aud` | If configured | Audience. Must match `audience` config. |
| `name` | Optional | Display name (used when `check_local_user = false`) |

### JWT Issuance Examples

**Python:**
```python
import jwt, time
token = jwt.encode(
    {"userId": "1001", "name": "Alice", "exp": int(time.time()) + 3600},
    "your-shared-hs256-secret",
    algorithm="HS256"
)
```

**Node.js:**
```javascript
const jwt = require('jsonwebtoken');
const token = jwt.sign(
    { userId: '1001', name: 'Alice' },
    'your-shared-hs256-secret',
    { expiresIn: '1h' }
);
```

### Client SDK Usage (cc-phone)

```typescript
CCPhone.create({
  server: 'wss://pbx.example.com/ws',
  agentId: '1001',
  jwt: '<your-jwt-token>',      // Fast registration via JWT
  // password: '...',           // Optional: only needed for Digest fallback
})
```

When `jwt` is provided, the SDK:
1. Appends `?token=<jwt>` to the WebSocket URL (path B pre-auth)
2. Includes `X-Auth-Token: <jwt>` header in SIP REGISTER (path A)

### Security Considerations

1. **Secret management**: The HS256 secret must be kept secure. Use environment variables or secrets management — do not commit it to source control.
2. **Token expiration**: Always set `exp` to limit token lifetime. Short-lived tokens (e.g. 1 hour) are recommended.
3. **Fallback safety**: Invalid or expired JWTs are silently ignored — the request falls back to Digest auth. This ensures backwards compatibility.
4. **WS pre-auth cleanup**: Pre-authenticated WebSocket connections are tracked in an in-memory registry. The entry is cleaned up when the connection closes.

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

## SSO Login Broker (Enterprise SSO → Native App)

Brokers an enterprise SSO login into a native-app deep link using the standard
OAuth2 authorization-code + PKCE flow (commerce builds only; endpoints mount
when `[sso].enabled = true`).

- Contracts: `docs/sso_upstream_integration.md` (upstream IdP) /
  `docs/sso_client_integration.md` (client app)
- End-to-end example with copy-paste commands:
  [sso_walkthrough.md](sso_walkthrough.md) · [English](sso_walkthrough_en.md)

Key interplay with the JWT fast registration above: share one HS256 secret
between `[sso.jwt]` and `[proxy.jwt_auth]` so tokens issued/relayed by the SSO
broker are accepted on the SIP (`X-Auth-Token`) and WebSocket (`?token=`)
chains unchanged. Stateful-free by design — cluster-safe with identical node
secrets, no shared storage.

## Push Notifications for Mobile Apps (RFC 8599)

Mobile apps such as Linphone cannot keep a SIP connection open while the OS
suspends or kills them. With `[proxy.push]` configured, RustPBX wakes them for
incoming calls with a push notification, following RFC 8599:

1. The app registers a Contact carrying `pn-provider`, `pn-prid` (its push
   token) and `pn-param`. RustPBX records the device (persisted in the
   `rustpbx_push_devices` table, so it survives restarts) and grants the
   binding up to `max_expires` instead of `max_registrar_expires`. The 200 OK
   carries `Feature-Caps: *;+sip.pns="fcm"`.
2. When the app's connection closes (the OS suspended or killed it), its
   binding stays registered: the device is asleep, not gone. Every lookup —
   direct calls, routes, queues, transfers, API-originated calls — still
   finds it, so none of them needs to know about push. (No `offline`
   locator event is emitted for it either.)
3. Every INVITE RustPBX sends passes through one place that recognizes push
   devices (by the `pn-*` parameters of the Request-URI, or by the INVITE
   going to a push device's connection). An INVITE to a push device always
   pushes it at once, whether its connection looks alive or not: a phone can
   sit on a half-open connection that the server cannot tell from a live one.
   The push carries the Call-ID of the INVITE.
4. If the device's connection is open (or the device uses UDP, whose
   liveness is unknown), the INVITE is sent on it right away. If the
   connection is down, the INVITE is held until the app sends its
   binding-refresh REGISTER. Each time the device registers on a new
   connection before it replies, the INVITE is sent again there, with the
   same Call-ID (the unanswered one is dropped). If the device does not
   re-register within `wakeup_timeout_secs` of the push, a held INVITE fails
   with 480; other devices of the callee ring in parallel meanwhile.
5. Shortly before a binding expires (`refresh_before_secs`), RustPBX pushes
   the device so it re-registers (RFC 8599 §5.5).

Push messages use the payload keys Flexisip sends (`call-id`, `loc-key`,
`send-time`, ...), so stock Linphone handles them: `call-id` is the Call-ID
of the INVITE the device is about to receive.

```toml
[proxy]
tcp_port = 5060   # or tls_port: push devices should not register over UDP

[proxy.push]
# enabled = true
# wakeup_timeout_secs = 30          # how long a call waits for a pushed device
# max_expires = 604800              # binding lifetime cap for push devices (7 days)
# refresh_before_secs = 300         # push devices this long before expiry; 0 disables
# reject_unsupported_provider = false  # 555 instead of registering without push
# include_caller_info = false       # caller URI/name in the payload (seen by the PNS)

[[proxy.push.fcm]]
service_account = "/etc/rustpbx/firebase-service-account.json"
# project_id = "my-app"             # defaults to the service account's project_id
# Linphone sends the Firebase project *number* (gcm_defaultSenderId) as
# pn-param; list it here. With a single [[proxy.push.fcm]] entry every fcm
# device is served by it regardless of pn-param.
aliases = ["123456789012"]

[[proxy.push.apns]]
key = "/etc/rustpbx/AuthKey_ABC123DEFG.p8"
key_id = "ABC123DEFG"
team_id = "DEF123GHIJ"
# bundle_ids = ["com.example.phone"]  # apps this key pushes to; empty: any
```

The service account needs permission to send FCM messages (e.g. the
"Firebase Cloud Messaging API Admin" role) in the Firebase project the app is
built with (its `google-services.json`).

### APNs (iOS)

One `[[proxy.push.apns]]` entry serves both APNs environments: devices that
register with `pn-provider=apns` go to production, `pn-provider=apns.dev`
(development builds) to the sandbox. Make the key in the Apple Developer
account (Certificates, Identifiers & Profiles > Keys, with "Apple Push
Notifications service"). It is a `.p8` file that can be downloaded once.
RustPBX signs a provider token (JWT) with it and renews it every 50 minutes.

Devices register the way Linphone does:

- `pn-param=<team ID>.<bundle ID>.voip&remote`
- `pn-prid=<VoIP token>:voip&<remote token>:remote`

The RFC 8599 form with a single token (`pn-param=<team>.<bundle>.voip`,
`pn-prid=<token>`) also works.

- Calls send a VoIP push (PushKit, topic `<bundle ID>.voip`). The payload
  holds `aps.call-id`, which Linphone uses to show the call in CallKit.
- Binding refreshes send a background push (`content-available`) to the
  remote token. iOS requires the app to report a CallKit call for every VoIP
  push, so RustPBX never uses one for a refresh. A device without a remote
  token gets no refresh pushes; iOS also limits background pushes to a few
  per hour.
- A device without a VoIP token is registered without push: a call could not
  wake it.
- Key selection: the entry whose `bundle_ids` lists the app, else the entry
  of the device's team ID with no `bundle_ids`. When only one entry is
  configured and it has no `bundle_ids`, it serves every APNs device. This
  is necessary for Linphone apps that do not set a team ID: they send the
  placeholder `ABCD1234`.
- `BadDeviceToken` usually means that the environment is wrong: a sandbox
  token registered with `apns`, or the other way round.

Notes:

- `fcm`, `apns` and `apns.dev` are implemented. Contacts naming another
  provider register as ordinary bindings, or get 555 with
  `reject_unsupported_provider = true`.
- A push token that FCM reports as `UNREGISTERED`, or that APNs answers with
  410 (`Unregistered`), is forgotten (the app was uninstalled).
- Only INVITEs wake devices; SIP MESSAGE to a sleeping device is not
  pushed.
- Device state lives on the node that handled the REGISTER; cluster
  deployments should route a user's calls through the same node.
- `[proxy.push]` is read at startup; changes need a restart.
- Metrics: `rustpbx_push_notifications_total{provider,kind,result}`,
  `rustpbx_push_wakeups_total{result}`,
  `rustpbx_push_wakeup_latency_seconds`.
