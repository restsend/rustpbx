# SSO Walkthrough

> 中文版：[sso_walkthrough.md](sso_walkthrough.md)

A complete, copy-pasteable example from zero to a working token: ops sets up
the config once, the enterprise SSO team aligns once, and the client developer
follows the flow.

> Prerequisites: rustpbx built with the `commerce` feature. The endpoints only
> exist when `[sso].enabled = true`.
>
> Wire contracts: `docs/sso_upstream_integration.md` (upstream IdP) and
> `docs/sso_client_integration.md` (client app).

## Roles

| Role | Responsibility |
|---|---|
| Operator (you) | Write the `[sso]` config; hand the callback URL and secret to the other parties |
| Enterprise SSO (IdP) | Provide the login page; redirect back to RustPBX with a JWT after login |
| Client developer | Generate PKCE pair → open browser → catch deep link → exchange code → call with token |

---

## Step 0: Server config (operator)

`config.toml`:

```toml
# SSO login broker (commerce build required)
[sso]
enabled      = true
base_path    = "/sso"
provider     = "jwt"
redirect_url = "myapp://auth/sso"     # deep link of the client app
auto_provision = false                # JIT-create role-less console user on first login

[sso.jwt]
secret             = "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X"   # delivered to the IdP out-of-band
issuer             = ""              # empty = no iss check; fill when the IdP has a fixed issuer
audience           = ""
user_id_claim      = "userId"        # claim that identifies the user in the enterprise JWT
upstream_login_url = "https://sso.corp.com/login?app=rustpbx"
token_mode         = "passthrough"   # client receives the enterprise JWT verbatim

# Let the SIP/WS chains accept SSO tokens (same secret as above)
[proxy.jwt_auth]
enabled        = true
secret         = "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X"
user_id_claim  = "userId"
check_local_user = false
```

Tell the enterprise SSO team two things:
1. Callback URL: `https://<your-domain>/sso/callback`
2. JWT contract (HS256, claims) — see `docs/sso_upstream_integration.md`

After restarting, verify the endpoint is mounted (anything but 404):

```bash
curl -si https://pbx.example.com/sso/authorize | head -n1
```

## Step 1: Client-side local prep

```bash
# code_verifier: 43-128 char URL-safe random string
VERIFIER=$(head -c 96 /dev/urandom | openssl base64 | tr '/+' '_-' | tr -d '=\n' | cut -c1-86)
# code_challenge = BASE64URL(SHA256(verifier))
CHALLENGE=$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary \
            | openssl base64 | tr '/+' '_-' | tr -d '=\n')
STATE=$(openssl rand -hex 16)
echo "$VERIFIER" > /tmp/sso_verifier  # needed for the exchange in step 2
echo "$STATE" > /tmp/sso_state        # kept for the CSRF check
```

Open the system browser (a real app launches it via custom-scheme handoff):

```
https://pbx.example.com/sso/authorize?code_challenge=<CHALLENGE>&code_challenge_method=S256&state=<STATE>
```

The browser lands on the enterprise SSO login page; after login it attempts
to navigate `myapp://auth/sso?code=...&state=...`.

**Extract `code` from the deep link and validate `state`:**

```bash
DEEP_LINK='myapp://auth/sso?code=ab12cd34ef56&state=e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0'
CODE=$(echo "$DEEP_LINK" | sed -E 's/.*[?&]code=([^&]+).*/\1/')
RSTATE=$(echo "$DEEP_LINK" | sed -E 's/.*[?&]state=([^&]+).*/\1/')
[ "$RSTATE" = "$(cat /tmp/sso_state)" ] || { echo "state mismatch!"; exit 1; }
```

## Step 2: Exchange the code

```bash
VERIFIER=$(cat /tmp/sso_verifier)
curl -s https://pbx.example.com/sso/token \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode grant_type=authorization_code \
  --data-urlencode "code=$CODE" \
  --data-urlencode "code_verifier=$VERIFIER"
```

Response (passthrough mode — access_token IS the enterprise JWT verbatim):

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
  "token_type": "Bearer",
  "expires_in": 287
}
```

## Step 3: Use the token (three chains)

```bash
TOKEN="eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

**REST API (Bearer)**

```bash
curl -s https://pbx.example.com/api/extensions \
  -H "Authorization: Bearer $TOKEN"
```

**SIP WebSocket**

```
wss://pbx.example.com/ws?token=<TOKEN>
```

**SIP registration / calls (REGISTER header)**

```sip
REGISTER sip:pbx.example.com SIP/2.0
X-Auth-Token: <TOKEN>
```

When the token expires (`expires_in`), silently re-run step 1: the IdP session
cookie is usually still alive so the browser round-trip completes in seconds.

---

## Recommended: exercise the real chain with the mock SSO server

`examples/test_sso_server.py` fully simulates the enterprise IdP contract
(login page, HS256 issuance, state echo, denial path) — pure standard library,
no external dependency:

```bash
python3 examples/test_sso_server.py \
    --port 9000 \
    --secret "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X" \
    --issuer "https://sso.example.com" \
    --callback-url "http://127.0.0.1:8088/sso/callback"

# Matching rustpbx config:
#   [sso.jwt] secret / issuer as above;
#   upstream_login_url = "http://127.0.0.1:9000/login?app=rustpbx"
# Add --auto for scripted runs (approve every login, no form).
```

Opening `http://<pbx>/sso/authorize?...` in a browser now lands on the mock
login page; submitting it returns the deep link — identical behavior to a
real enterprise SSO.

## Appendix: browserless self-test (verify the server before the IdP joins)

The following exercises the whole chain entirely from the command line,
without the mock server:

```bash
#!/usr/bin/env bash
set -euo pipefail
PBX=http://127.0.0.1:8088          # your PBX address
SECRET=s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X

# matching PKCE pair
VERIFIER=$(head -c 96 /dev/urandom | openssl base64 | tr '/+' '_-' | tr -d '=\n' | cut -c1-86)
CHALLENGE=$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary \
            | openssl base64 | tr '/+' '_-' | tr -d '=\n')

# 1. Mint a test "enterprise JWT" (what the IdP would emit)
UPSTREAM_JWT=$(python3 - <<EOF
import base64, hashlib, hmac, json, time
def b64(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
h=b64(json.dumps({"alg":"HS256","typ":"JWT"}).encode())
p=b64(json.dumps({"userId":"1001","email":"alice@corp.com",
                  "exp":int(time.time())+300}).encode())
sig=b64(hmac.new(b"$SECRET", f"{h}.{p}".encode(), hashlib.sha256).digest())
print(f"{h}.{p}.{sig}")
EOF
)

# 2. Kick off an authorization; capture the state envelope sent upstream
LOC=$(curl -si "$PBX/sso/authorize?code_challenge=$CHALLENGE&code_challenge_method=S256&state=demo" \
      | grep -i '^location:' | tr -d '\r' | awk '{print $2}')
FLOW_STATE=$(echo "$LOC" | sed -E 's/.*[?&]state=([^&]+).*/\1/')
echo "flow envelope: ${FLOW_STATE:0:40}..."

# 3. Simulate the post-login redirect from the enterprise SSO
LOC2=$(curl -si "$PBX/sso/callback?token=$UPSTREAM_JWT&state=$FLOW_STATE" \
       | grep -i '^location:' | tr -d '\r' | awk '{print $2}')
echo "deep link: $LOC2"

CODE=$(echo "$LOC2" | sed -E 's/.*[?&]code=([^&]+).*/\1/')

# 4. Redeem the code (only passes when PKCE verifies)
curl -s "$PBX/sso/token" \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  --data-urlencode grant_type=authorization_code \
  --data-urlencode "code=$CODE" \
  --data-urlencode "code_verifier=$VERIFIER" | jq .
```

### Expected output

Step 2 prints the sealed envelope, step 3 prints the deep link, and step 4
returns:

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",  // identical to UPSTREAM_JWT
  "token_type": "Bearer",
  "expires_in": 29x
}
```

## Troubleshooting

| Symptom | Cause |
|---|---|
| Endpoints return 404 | Not enabled: `[sso].enabled` or missing commerce feature |
| `/authorize` 400 `missing code_challenge` | Missing/empty parameter |
| `/callback` 400 `invalid or expired state` | Flow envelope expired (`flow_ttl_secs`), reused, or secrets differ between nodes |
| `/token` `invalid_grant` | Code expired (`code_ttl_secs`) or **code_verifier does not match the challenge** |
| `/api` 401 with `no local user` in logs | `auto_provision=false` and the identity maps to no local user |
