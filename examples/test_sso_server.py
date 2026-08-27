#! /usr/bin/env python3
"""Mock enterprise SSO server for testing the rustpbx SSO login broker.

Simulates the upstream IdP side of the JWT handoff contract
(docs/sso_upstream_integration.md):

  1. RustPBX 302s the browser here:   {--login-path}?state=<sealed envelope>
  2. The user "logs in" (any credentials are accepted by default)
  3. This server mints an HS256 JWT and 302s back:
        {--callback-url}?token=<jwt>&state=<echoed verbatim>

Pure standard library — no dependencies.

Usage:
  python3 examples/test_sso_server.py \
      --port 9000 \
      --secret "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X" \
      --issuer "https://mock-sso.corp.com" \
      --callback-url "http://127.0.0.1:8088/sso/callback"

Corresponding rustpbx config (the secret/issuer must match):

  [sso]
  enabled = true
  redirect_url = "myapp://auth/sso"

  [sso.jwt]
  secret             = "s6cQ8vTpL2mZx1WdKfNq7RyUeGhJbA9X"
  issuer             = "https://mock-sso.corp.com"     # must equal --issuer if non-empty
  user_id_claim      = "userId"
  upstream_login_url = "http://127.0.0.1:9000/login?app=rustpbx"
  token_mode         = "passthrough"

Scripted (browserless) run against a live PBX:

  python3 examples/test_sso_server.py --auto &       # auto-approve logins

  LOC=$(curl -si 'http://127.0.0.1:8088/sso/authorize?code_challenge=x&code_challenge_method=S256&state=demo' \
        | grep -i ^location: | awk '{print $2}')                 # -> mock /login?state=...
  curl -si "$LOC&auto=1" | grep -i ^location:                    # follow the chain manually or
                                                                 # let the browser do it end-to-end
"""

import argparse
import base64
import hashlib
import hmac
import json
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = None  # populated from main()

LOGIN_PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>Mock SSO Login</title>
<style>
 body {{ font-family: -apple-system, sans-serif; background:#f4f5f7;
        display:flex; align-items:center; justify-content:center; height:100vh; margin:0 }}
 .card {{ background:#fff; padding:2rem 2.5rem; border-radius:12px;
          box-shadow:0 2px 10px rgba(0,0,0,.08); width:320px }}
 input {{ width:100%; padding:.5rem; margin:.35rem 0 1rem; box-sizing:border-box }}
 button {{ width:100%; padding:.6rem; background:#2563eb; color:#fff;
           border:none; border-radius:6px; cursor:pointer }}
 small {{ color:#888 }}
</style></head>
<body><div class="card">
 <h3>Mock SSO Login</h3>
 <small>state={state_short}… · issuer={issuer}</small>
 <form method="POST" action="/login">
   <input type="hidden" name="state" value="{state}">
   User ID:  <input name="user_id" value="1001">
   Email:    <input name="email" value="alice@corp.com">
   Name:     <input name="name" value="Alice">
   <button type="submit">Sign in</button>
 </form>
</div></body></html>"""


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def mint_jwt(secret: str, claims: dict) -> str:
    header = b64url(json.dumps({"alg": "HS256", "typ": "JWT"},
                               separators=(",", ":")).encode())
    payload = b64url(json.dumps(claims, separators=(",", ":")).encode())
    signing_input = f"{header}.{payload}"
    sig = hmac.new(secret.encode(), signing_input.encode(), hashlib.sha256).digest()
    return f"{signing_input}.{b64url(sig)}"


class Handler(BaseHTTPRequestHandler):
    def _redirect(self, location: str) -> None:
        self.send_response(302)
        self.send_header("Location", location)
        self.end_headers()

    def send_login_form(self, state: str) -> None:
        page = LOGIN_PAGE.format(
            state=state,
            state_short=state[:24],
            issuer=ARGS.issuer,
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(page)))
        self.end_headers()
        self.wfile.write(page)

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        qs = urllib.parse.parse_qs(parsed.query)

        if parsed.path == "/health":
            body = b'{"status":"ok"}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        # Both /login (human) and /login?...&auto=1 (scripted) supported.
        state = qs.get("state", [""])[0]
        if not state:
            self._redirect(f"{ARGS.callback_url}?error=request_denied")
            return
        if ARGS.auto or qs.get("auto", ["0"])[0] == "1":
            self.issue_and_redirect(state, {
                ARGS.user_id_claim: ARGS.user_id,
                "email": ARGS.email,
                "name": "Mock User",
            })
            return
        self.send_login_form(state)

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path != "/login":
            self.send_error(404)
            return

        length = int(self.headers.get("Content-Length", "0"))
        form = urllib.parse.parse_qs(
            self.rfile.read(length).decode(), keep_blank_values=True
        )
        state = form.get("state", [""])[0]
        if not state:
            self._redirect(f"{ARGS.callback_url}?error=request_denied")
            return

        user_id = form.get("user_id", [ARGS.user_id])[0].strip() or ARGS.user_id
        email = form.get("email", [ARGS.email])[0].strip() or ARGS.email
        name = form.get("name", [""])[0].strip()

        if "deny" in form:  # simulated "cancel" button
            self.issue_denial(state)
            return

        claims = {ARGS.user_id_claim: user_id, "email": email}
        if name:
            claims["name"] = name
        if ARGS.agent_id:
            claims["agent_id"] = ARGS.agent_id
        if ARGS.mis_id:
            claims["mis_id"] = ARGS.mis_id
        self.issue_and_redirect(state, claims)

    def issue_and_redirect(self, state: str, extra_claims: dict) -> None:
        now = int(time.time())
        claims = {"iss": ARGS.issuer, "iat": now, "exp": now + ARGS.ttl}
        claims.update({k: v for k, v in extra_claims.items() if v})
        token = mint_jwt(ARGS.secret, claims)
        print(f"[mock-sso] issued token for {extra_claims!r} ttl={ARGS.ttl}s")
        q = urllib.parse.urlencode({"token": token, "state": state})
        self._redirect(f"{ARGS.callback_url}?{q}")

    def issue_denial(self, state: str) -> None:
        print("[mock-sso] user denied login")
        self._redirect(f"{ARGS.callback_url}?error=access_denied&state={urllib.parse.quote(state)}")

    def log_message(self, fmt, *args):  # quieter default logging
        print(f"[mock-sso] {self.address_string()} {fmt % args}")


def main():
    global ARGS
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    parser.add_argument("--port", type=int, default=9000)
    parser.add_argument("--secret", default="dev-secret",
                        help="HS256 shared secret; must match [sso.jwt].secret")
    parser.add_argument("--issuer", default="https://mock-sso.corp.com",
                        help="must match [sso.jwt].issuer when set there")
    parser.add_argument("--callback-url", required=True,
                        help="PBX callback, e.g. http://127.0.0.1:8088/sso/callback")
    parser.add_argument("--user-id-claim", default="userId",
                        help="must match [sso.jwt].user_id_claim")
    parser.add_argument("--user-id", default="1001", help="default user id (auto mode)")
    parser.add_argument("--email", default="alice@corp.com", help="default email (auto mode)")
    parser.add_argument("--ttl", type=int, default=300,
                        help="JWT lifetime seconds (keep short; it is an entry ticket)")
    parser.add_argument("--agent-id", default="", help="optional agent_id claim passthrough demo")
    parser.add_argument("--mis-id", default="", help="optional mis_id claim passthrough demo")
    parser.add_argument("--auto", action="store_true",
                        help="skip the HTML form; approve every login immediately")
    ARGS = parser.parse_args()

    server = ThreadingHTTPServer(("0.0.0.0", ARGS.port), Handler)
    print(f"[mock-sso] listening on :{ARGS.port}")
    print(f"[mock-sso] login URL for [sso.jwt].upstream_login_url:")
    print(f"           http://127.0.0.1:{ARGS.port}/login?app=rustpbx")
    print(f"[mock-sso] callback target: {ARGS.callback_url}")
    print(f"[mock-sso] issuer={ARGS.issuer} claim={ARGS.user_id_claim} ttl={ARGS.ttl}s "
          f"auto={ARGS.auto}")
    server.serve_forever()


if __name__ == "__main__":
    main()
