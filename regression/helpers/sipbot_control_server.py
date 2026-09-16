"""SipBot control HTTP server for cc-phone E2E widget tests.

Provides REST endpoints to spawn and manage sipbot processes for real
SIP interactions with the cc-phone widget. Mirrors the Node.js control
server from cc-phone/e2e/server.js.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import tempfile
import time
from typing import Optional

from aiohttp import web

from .sipbot import SipBotProcess

logger = logging.getLogger(__name__)


class SipBotControlServer:
    """Manages sipbot subprocesses via HTTP endpoints."""

    def __init__(
        self,
        *,
        host: str = "127.0.0.1",
        port: int = 0,
        sip_host: str = "127.0.0.1",
        sip_port: int = 5060,
        phone_password: str = "123456",
    ):
        self.host = host
        self.port = port
        self.sip_host = sip_host
        self.sip_port = sip_port
        self.phone_password = phone_password
        self._procs: list[SipBotProcess] = []
        self._runner: Optional[web.AppRunner] = None
        self._site: Optional[web.TCPSite] = None
        self._actual_port: Optional[int] = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None

    @property
    def url(self) -> str:
        p = self._actual_port or self.port
        return f"http://{self.host}:{p}"

    async def start(self) -> None:
        self._loop = asyncio.get_event_loop()
        app = web.Application()
        app.router.add_post("/control/call", self._handle_call)
        app.router.add_post("/control/register-listener", self._handle_register_listener)
        app.router.add_post("/control/cleanup", self._handle_cleanup)
        app.router.add_get("/health", self._handle_health)

        # Serve static test page
        test_html = _build_test_page_html(self.sip_host, self.sip_port)
        async def serve_test_page(request):
            return web.Response(text=test_html, content_type="text/html")
        app.router.add_get("/", serve_test_page)

        self._runner = web.AppRunner(app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, self.host, self.port)
        await self._site.start()
        self._actual_port = self._site._server.sockets[0].getsockname()[1]
        logger.info("SipBot control server on %s", self.url)

    async def stop(self) -> None:
        self._cleanup_all()
        if self._runner:
            await self._runner.cleanup()
            self._runner = None

    async def _handle_call(self, request: web.Request) -> web.Response:
        data = await request.json()
        target = data.get("target", "1001")
        username = data.get("username", "1002")
        proc = SipBotProcess(name=f"ctrl-call-{target}")
        proc.start_caller(
            target=f"sip:{target}@{self.sip_host}:{self.sip_port}",
            username=username,
            password=self.phone_password,
            codecs="pcmu",
            hangup=8,
        )
        self._procs.append(proc)
        logger.info("Spawned sipbot call to %s (pid=%s)", target, proc.process.pid if proc.process else "?")
        return web.json_response({"ok": True, "pid": str(proc.process.pid) if proc.process else "", "target": target})

    async def _handle_register_listener(self, request: web.Request) -> web.Response:
        data = await request.json()
        username = data.get("username", "1002")
        port = data.get("port", 15160)
        proc = SipBotProcess(name=f"ctrl-listener-{username}")
        proc.start_callee(
            host=self.sip_host,
            port=port,
            username=username,
            password=self.phone_password,
            register=True,
            proxy=f"{self.sip_host}:{self.sip_port}",
            domain=self.sip_host,
            ring_secs=1,
            answer_mode="echo",
        )
        self._procs.append(proc)
        await asyncio.sleep(1.5)  # Wait for SIP registration
        logger.info("Registered sipbot listener %s on port %s", username, port)
        return web.json_response({"ok": True, "pid": str(proc.process.pid) if proc.process else "", "username": username})

    async def _handle_cleanup(self, request: web.Request) -> web.Response:
        count = self._cleanup_all()
        return web.json_response({"ok": True, "killed": count})

    async def _handle_health(self, request: web.Request) -> web.Response:
        return web.json_response({"ok": True, "active": len(self._procs)})

    def _cleanup_all(self) -> int:
        count = 0
        for p in self._procs:
            if p.is_alive:
                p.terminate()
                count += 1
        self._procs.clear()
        return count


def _build_test_page_html(sip_host: str, sip_port: int) -> str:
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>cc-phone E2E Test</title>
<style>
  body {{ font-family: sans-serif; padding: 20px; background: #1e293b; color: #e2e8f0; }}
  #cc-phone-e2e-container {{ width: 100%; max-width: 400px; }}
  #agent-status {{ padding: 8px; font-size: 14px; color: #94a3b8; }}
</style>
</head>
<body>
<h1>cc-phone E2E Test Page</h1>
<div id="agent-status">loading...</div>
<div id="cc-phone-e2e-container"></div>
</body>
</html>
"""
