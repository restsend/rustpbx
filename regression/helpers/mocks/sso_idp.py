"""Mock enterprise SSO IdP (login broker JWT handoff) — pytest-friendly port of
examples/test_sso_server.py. Modes:
    auto  — ?auto=1 on the login page mints + redirects immediately (scripts)
    deny  — deny button path (reject flow)
Serves a minimal login form, mints HS256 JWT (iss/iat/exp + user_id claim +
optional agent_id/mis_id) and 302-redirects to --callback-url with token+state.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import time
import urllib.parse
from typing import Optional

from aiohttp import web

LOGIN_PAGE = """<!doctype html><html><body style="font-family:sans-serif">
<h3>RustPBX mock IdP</h3>
<form method="post" action="/login">
  <input name="user_id" placeholder="user id" value="{user}"/>
  <input name="email" placeholder="email" value="user@example.com"/>
  <input name="name" placeholder="display name" value="E2E User"/>
  <input type="hidden" name="state" value="{state}"/>
  <button name="action" value="login">login</button>
  <button name="action" value="deny">deny</button>
</form>
<p><a href="/login?auto=1&state={state}">scripted auto-login</a></p>
</body></html>"""


class SsoIdpMock:
    def __init__(self, *, secret: str = "e2e-sso-secret", issuer: str = "mock-idp",
                 callback_url: str = "http://127.0.0.1:18080/sso/callback",
                 user_id_claim: str = "preferred_username", agent_id: Optional[str] = None,
                 mis_id: Optional[str] = None, expires_s: int = 300):
        self.secret = secret.encode()
        self.issuer = issuer
        self.callback_url = callback_url
        self.user_id_claim = user_id_claim
        self.agent_id = agent_id
        self.mis_id = mis_id
        self.expires_s = expires_s
        self.logins: list[dict] = []
        self.denied: list[dict] = []
        self._runner: Optional[web.AppRunner] = None
        self.base_url = ""

    # -- JWT (HS256, stdlib only) ------------------------------------------

    def mint_jwt(self, user_id: str, extra: Optional[dict] = None) -> str:
        def b64(obj) -> str:
            raw = json.dumps(obj, separators=(",", ":")).encode()
            return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()

        now = int(time.time())
        header = b64({"alg": "HS256", "typ": "JWT"})
        payload = b64({
            "iss": self.issuer, "iat": now, "exp": now + self.expires_s,
            self.user_id_claim: user_id,
            **({"agent_id": self.agent_id} if self.agent_id else {}),
            **({"mis_id": self.mis_id} if self.mis_id else {}),
            **(extra or {}),
        })
        signing = f"{header}.{payload}".encode()
        sig = base64.urlsafe_b64encode(hmac.new(self.secret, signing, hashlib.sha256).digest()).rstrip(b"=").decode()
        return f"{header}.{payload}.{sig}"

    # -- handlers -----------------------------------------------------------

    async def _login_get(self, request: web.Request):
        state = request.query.get("state", "")
        if request.query.get("auto") == "1":
            return await self._redirect_with_token("auto-user", state)
        return web.Response(text=LOGIN_PAGE.format(state=urllib.parse.quote(state), user="auto-user"), content_type="text/html")

    async def _login_post(self, request: web.Request):
        form = await request.post()
        record = {k: form.get(k, "") for k in ("user_id", "email", "name", "state", "action")}
        if record.get("action") == "deny":
            self.denied.append(record)
            return web.Response(text="login denied by IdP", status=403)
        self.logins.append(record)
        return await self._redirect_with_token(str(record.get("user_id") or "user"), str(record.get("state") or ""))

    async def _redirect_with_token(self, user_id: str, state: str):
        token = self.mint_jwt(user_id)
        sep = "&" if "?" in self.callback_url else "?"
        target = f"{self.callback_url}{sep}token={urllib.parse.quote(token)}&state={urllib.parse.quote(state)}"
        raise web.HTTPFound(target)

    async def start(self) -> str:
        app = web.Application()
        app.router.add_get("/login", self._login_get)
        app.router.add_post("/login", self._login_post)
        app.router.add_get("/health", lambda r: web.json_response({"ok": True}))
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        self.base_url = f"http://127.0.0.1:{port}"
        return self.base_url

    async def stop(self):
        if self._runner:
            await self._runner.cleanup()
            self._runner = None
