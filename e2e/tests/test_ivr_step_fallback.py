"""Step IVR `/fail` + `[proxy.ivr_fallback]` SIP E2E tests.

Covers the recovery ladder end-to-end against a real rustpbx + sipbot:

1. Node execute fails (tree-only `repeat`) → `POST /fail` returns transfer
2. `/fail` also fails → match-rule fallback tree IVR transfers to agent
3. `/step` unreachable → default fallback IVR transfers to agent
"""

from __future__ import annotations

import asyncio

import pytest
from aiohttp import web

import helpers as h

pytestmark = [pytest.mark.ivr]


_FALLBACK_TREE = """\
[ivr]
name = "fallback-tree"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = ""
timeout_ms = 1000
max_retries = 0
max_retries_action = { type = "transfer", target = "1002" }
entries = []
"""


def _start_step_provider(*, step_handler, fail_handler=None):
    """Scripted provider with `/ivr/step` and `/ivr/step/fail`.

    ``fail_handler`` returning ``None`` yields HTTP 503 (fail endpoint down).
    """
    hits: list[dict] = []

    async def handle_step(request):
        body = await request.json()
        hits.append({"path": "step", "body": body})
        result = step_handler(body)
        if result is None:
            return web.Response(status=503, text="step unavailable")
        return web.json_response(result)

    async def handle_fail(request):
        body = await request.json()
        hits.append({"path": "fail", "body": body})
        if fail_handler is None:
            return web.Response(status=503, text="fail unavailable")
        result = fail_handler(body)
        if result is None:
            return web.Response(status=503, text="fail unavailable")
        return web.json_response(result)

    async def handle_ok(request):
        # /start and /end are fire-and-forget
        try:
            body = await request.json()
        except Exception:
            body = {}
        hits.append({"path": request.path.rsplit("/", 1)[-1], "body": body})
        return web.json_response({"ok": True})

    app = web.Application()
    app.router.add_post("/ivr/step", handle_step)
    app.router.add_post("/ivr/step/fail", handle_fail)
    app.router.add_post("/ivr/step/start", handle_ok)
    app.router.add_post("/ivr/step/end", handle_ok)
    runner = web.AppRunner(app)

    async def start():
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        return f"http://127.0.0.1:{port}/ivr/step"

    async def cleanup():
        await runner.cleanup()

    return hits, start, cleanup


def _add_step_route(pbx, route_name, match_user, url):
    pbx.config_builder.add_route(
        route_name,
        match={"to.user": match_user},
        priority=10,
        action="application",
        app="ivr",
        app_params={"mode": "step", "url": url},
        auto_answer=True,
    )


async def _reg_callee(sipbot_pool, pbx, port, username):
    ua = sipbot_pool.callee(
        host=pbx.host,
        port=port,
        username=username,
        password="123456",
        register=True,
        proxy=f"{pbx.host}:{pbx.sip_port}",
        domain=pbx.host,
        ring_secs=1,
        answer_mode="echo",
        hangup_after=8,
    )
    await h.wait_registered(ua)
    return ua


def _fail_events(hits: list[dict]) -> list[dict]:
    out = []
    for hitem in hits:
        if hitem.get("path") != "fail":
            continue
        ev = ((hitem.get("body") or {}).get("event")) or {}
        if ev.get("type") == "fail":
            out.append(ev)
    return out


# ---------------------------------------------------------------------------
# /fail recovers in the same step session
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_fail_recovers_and_transfers(pbx, sipbot_pool):
    """Tree-only `repeat` fails execute → `/fail` returns transfer → agent answers."""

    def step_handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "repeat"}  # not valid in step mode → execute Err
        return {"type": "hangup"}

    def fail_handler(body):
        ev = (body or {}).get("event") or {}
        assert ev.get("type") == "fail", body
        return {"type": "transfer", "target": "1002"}

    hits, start, cleanup = _start_step_provider(
        step_handler=step_handler, fail_handler=fail_handler
    )
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-fail-ok", "ivr-fail-ok", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, h.ua_port(15150), "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-fail-ok@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=20,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
            caller.output
        )
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=30), (
            f"agent never got /fail recovery transfer:\n{agent.output[-1500:]}\n"
            f"provider hits={hits}"
        )
        assert _fail_events(hits), f"expected /fail hit, got {hits}"
    finally:
        await cleanup()


# ---------------------------------------------------------------------------
# /fail down → match-rule fallback IVR
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_fail_then_fallback_rule(pbx, sipbot_pool):
    """`/fail` 503 → `[proxy.ivr_fallback]` rule matches caller → tree IVR → 1002."""

    def step_handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "repeat"}
        return {"type": "hangup"}

    hits, start, cleanup = _start_step_provider(
        step_handler=step_handler, fail_handler=lambda _b: None
    )
    try:
        url = await start()
        pbx.config_builder.add_ivr("fallback-vip", _FALLBACK_TREE)
        pbx.config_builder.set_ivr_fallback(
            default="fallback-default-unused",
            rules=[
                {
                    "name": "vip",
                    "priority": 100,
                    "match": {"from.user": "1001"},
                    "target": "fallback-vip",
                }
            ],
        )
        _add_step_route(pbx, "to-ivr-fail-fb", "ivr-fail-fb", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, h.ua_port(15151), "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-fail-fb@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=25,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
            caller.output
        )
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=35), (
            f"agent never got fallback-rule transfer:\n{agent.output[-1500:]}\n"
            f"provider hits={hits}"
        )
        assert _fail_events(hits), f"expected /fail attempt before fallback, got {hits}"
    finally:
        await cleanup()


# ---------------------------------------------------------------------------
# /step unreachable → default fallback IVR
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_step_down_uses_default_fallback(pbx, sipbot_pool):
    """`/step` always 503 → prefer ivr_fallback default tree → transfer 1002."""

    def step_handler(_body):
        return None  # 503

    hits, start, cleanup = _start_step_provider(
        step_handler=step_handler, fail_handler=None
    )
    try:
        url = await start()
        pbx.config_builder.add_ivr("fallback-default", _FALLBACK_TREE)
        pbx.config_builder.set_ivr_fallback(default="fallback-default", rules=[])
        _add_step_route(pbx, "to-ivr-step-down", "ivr-step-down", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, h.ua_port(15152), "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-step-down@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=40,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
            caller.output
        )
        # /step retries (default 3 × ~1s) then JumpIvr → tree timeout → transfer
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=45), (
            f"agent never got default-fallback transfer:\n{agent.output[-1500:]}\n"
            f"provider hits={hits}"
        )
        step_hits = [hitem for hitem in hits if hitem.get("path") == "step"]
        assert step_hits, f"expected /step attempts, got {hits}"
    finally:
        await cleanup()
