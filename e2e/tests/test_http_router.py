"""HTTP router E2E tests.

Covers the proxy-level `[proxy.http_router]` (CallRouter POST + response) and
the step-mode IVR HTTP provider (app=ivr, mode=step). A mock aiohttp server
acts as the router / provider.
"""

from __future__ import annotations

import asyncio
import uuid
from typing import Optional

import aiohttp
import pytest
from aiohttp import web

import helpers as h

pytestmark = [pytest.mark.http_router]


class MockHttpRouter:
    """Minimal aiohttp router/provider; records POST bodies."""

    def __init__(self):
        self.requests: list[dict] = []
        self.response: dict = {"action": "forward"}
        self._runner: Optional[web.AppRunner] = None
        self.port: int = 0

    async def _handle(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.requests.append(body)
        return web.json_response(self.response)

    async def start(self) -> None:
        app = web.Application()
        app.router.add_post("/route", self._handle)
        app.router.add_post("/ivr", self._handle)
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await self._site.start()
        self.port = self._site._server.sockets[0].getsockname()[1]

    async def stop(self) -> None:
        if self._runner:
            await self._runner.cleanup()


@pytest.fixture
async def http_router():
    server = MockHttpRouter()
    await server.start()
    yield server
    await server.stop()


async def _reg_callee(sipbot_pool, pbx, port):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await asyncio.sleep(2)
    return ua


@pytest.mark.xfail(reason="WIP media layer: http_router forward bridge media allocation fails with WebRTC callers (500)")
@pytest.mark.asyncio
async def test_http_router_forwards_call(pbx, http_router, sipbot_pool):
    """[proxy.http_router] consulted for a call; response routes to the target."""
    http_router.response = {"action": "forward", "targets": ["sip:1002@127.0.0.1:15160"]}
    pbx.config_builder.enable_http_router(f"http://127.0.0.1:{http_router.port}/route")
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, 15160)
    caller = sipbot_pool.caller(
        target=f"sip:anyone@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    assert http_router.requests, "http_router was not consulted"
    payload = http_router.requests[0]
    assert "caller" in payload and "to" in payload, payload


@pytest.mark.asyncio
async def test_http_router_reject(pbx, http_router, sipbot_pool):
    """http_router response action=reject -> call rejected (no RTP)."""
    http_router.response = {"action": "reject", "status": 403, "reason": "blocked"}
    pbx.config_builder.enable_http_router(f"http://127.0.0.1:{http_router.port}/route")
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:anyone@{pbx.sip_addr}", username="1001", password="123456", hangup=4,
    )
    await caller.wait_output_async(r"403|Rejected|4[0-9][0-9]", timeout=20)
    assert not caller.get_rtp_stats().has_rx, "rejected call should have no RTP"
    assert http_router.requests, "http_router was not consulted"


@pytest.mark.asyncio
async def test_ivr_step_provider(pbx, http_router, sipbot_pool):
    """Step-mode IVR driven by an HTTP provider returns actions (prompt -> queue)."""
    http_router.response = {
        "action": "prompt",
        "text": "Welcome to step IVR.",
        "next": {"action": "transfer", "target": "1002"},
    }
    pbx.config_builder.add_route(
        "to-step-ivr",
        match={"to.user": "ivr"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"mode": "step", "url": f"http://127.0.0.1:{http_router.port}/ivr"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, 15161)
    caller = sipbot_pool.caller(
        target=f"sip:ivr@{pbx.sip_addr}", username="1001", password="123456", hangup=8,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 25)
    assert http_router.requests, "step provider was not consulted"
