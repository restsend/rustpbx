"""Vision interface mock (CC routing dependency): returns the skill group /
agent target for a caller, with injectable failure modes for acceptance rows:
    - normal     -> {"skill_group": "support", "agent": null, ...}
    - fail       -> 503 (Vision unreachable)
    - badtarget  -> non-existent agent id (toagent_by_kb failure scenario)

Wired as the [proxy.http_router]-style or ACD external dependency; e2e tests
point the CC config at mock.base_url and assert fallback behavior.
"""

from __future__ import annotations

import threading
from typing import Optional

from aiohttp import web


class VisionServerMock:
    def __init__(self):
        # default routing table: caller number -> target
        self.routes: dict[str, dict] = {}
        self.default: dict = {"skill_group": "support"}
        self.mode = "normal"  # normal | fail | badtarget | timeout
        self.requests: list[dict] = []
        self._lock = threading.Lock()
        self._runner: Optional[web.AppRunner] = None
        self.base_url = ""

    async def _handle(self, request: web.Request):
        try:
            body = await request.json()
        except Exception:
            body = dict(request.query)
        with self._lock:
            self.requests.append(body)
            mode = self.mode
        if mode == "fail":
            return web.json_response({"error": "vision injected failure"}, status=503)
        if mode == "timeout":
            import asyncio

            await asyncio.sleep(30)
            return web.json_response({"error": "timeout"}, status=504)
        caller = str(body.get("caller") or body.get("from") or body.get("user") or "")
        target = self.routes.get(caller, self.default)
        if mode == "badtarget":
            target = {"skill_group": "nonexistent-sg-404", "agent": "ghost-agent"}
        return web.json_response({"ok": True, **target})

    async def start(self) -> str:
        app = web.Application()
        app.router.add_post("/vision/route", self._handle)
        app.router.add_get("/vision/route", self._handle)
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
