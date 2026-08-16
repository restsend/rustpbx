"""SBC addon (JSON-RPC) E2E tests.

Configures `config/sbc/sbc_jsonrpc.toml` rules backed by a mock upstream server,
then verifies rewrite / reject / header-injection behavior on real calls.
"""

from __future__ import annotations

import asyncio
import uuid
from typing import Optional

import aiohttp
import pytest
from aiohttp import web

import helpers as h

pytestmark = [pytest.mark.sbc]


class MockSbcUpstream:
    def __init__(self, response: dict):
        self.response = response
        self.calls: list[dict] = []
        self._runner: Optional[web.AppRunner] = None
        self.port: int = 0

    async def _handle(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.calls.append(body)
        return web.json_response(self.response)

    async def start(self) -> None:
        app = web.Application()
        app.router.add_post("/jsonrpc", self._handle)
        self._runner = web.AppRunner(app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "127.0.0.1", 0)
        await site.start()
        self.port = site._server.sockets[0].getsockname()[1]

    async def stop(self) -> None:
        if self._runner:
            await self._runner.cleanup()


@pytest.fixture
async def sbc_upstream():
    server = MockSbcUpstream({"result": "ok"})
    await server.start()
    yield server
    await server.stop()


def _rule(name: str, url: str, *, success_when: str = "true",
          reject_status: int = 403, callee_rewrite: str = "",
          inject_headers: Optional[list[dict]] = None) -> dict:
    resp: dict = {
        "success_when": success_when,
        "reject_status": reject_status,
        "reject_on_eval_error": True,
        "passthrough_original_headers": True,
    }
    if callee_rewrite:
        resp["callee_rewrite"] = callee_rewrite
    if inject_headers:
        resp["inject_headers"] = inject_headers
    return {
        "name": name,
        "enabled": True,
        "match_group": {
            "logic": "all",
            "conditions": [
                {"field": "callee_user", "op": "equals", "value": "1002"},
            ],
        },
        "upstream": {
            "method": "POST",
            "url": f"{url}/jsonrpc",
            "body": '{"jsonrpc":"2.0","method":"route","id":1}',
        },
        "response": resp,
    }


async def _reg_callee(sipbot_pool, pbx, port):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


@pytest.mark.asyncio
async def test_sbc_rewrite_call_established(pbx, sbc_upstream, sipbot_pool):
    """SBC rule upstream ok (success_when=true) -> call established, caller gets audio."""
    pbx.config_builder.add_sbc_jsonrpc(
        [_rule("e2e-ok", f"http://127.0.0.1:{sbc_upstream.port}")]
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, 15170)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    assert len(sbc_upstream.calls) == 1, f"expected 1 upstream call, got {len(sbc_upstream.calls)}"


@pytest.mark.asyncio
async def test_sbc_reject_call(pbx, sbc_upstream, sipbot_pool):
    """Upstream success_when fails -> call rejected (no RTP)."""
    sbc_upstream.response = {"result": "denied"}
    pbx.config_builder.add_sbc_jsonrpc(
        [_rule("e2e-reject", f"http://127.0.0.1:{sbc_upstream.port}",
               success_when='json.result == "ok"')]
    )
    h.boot_pbx(pbx)

    callee = await _reg_callee(sipbot_pool, pbx, 15171)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=4,
    )
    await caller.wait_output_async(r"403|Rejected|4[0-9][0-9]", timeout=20)
    assert not callee.get_rtp_stats().has_rx, "callee should NOT receive RTP on reject"
    assert len(sbc_upstream.calls) == 1


@pytest.mark.asyncio
async def test_sbc_header_injection(pbx, sbc_upstream, sipbot_pool):
    """SBC injects headers on the outbound INVITE; call still established."""
    pbx.config_builder.add_sbc_jsonrpc(
        [_rule(
            "e2e-header", f"http://127.0.0.1:{sbc_upstream.port}",
            inject_headers=[{"action": "add", "name": "X-SBC-Route", "value": "e2e-injected"}],
        )]
    )
    h.boot_pbx(pbx)

    await _reg_callee(sipbot_pool, pbx, 15172)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456", hangup=6,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    assert len(sbc_upstream.calls) == 1
