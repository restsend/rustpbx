"""Outbound dial SSE interface E2E tests (Python + sipbot).

Tests the POST /ami/v1/outbound/dial endpoint end-to-end. The SSE stream is
pure RWI event passthrough — assertions use RWI event type names:
  call_created, call_ringing, call_answered, call_busy, call_no_answer, etc.

Requires the `sipbot` binary in PATH (cargo install sipbot).
"""

from __future__ import annotations

import asyncio
import json
import uuid

import aiohttp
import pytest

import helpers as h

pytestmark = [pytest.mark.outbound]

# RWI event types that terminate the SSE stream.
_TERMINAL_EVENTS = frozenset({
    "call_answered", "call_busy", "call_no_answer", "call_hangup",
})


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


async def _sse_dial(
    session: aiohttp.ClientSession,
    http_url: str,
    body: dict,
    timeout: float = 30,
) -> list[dict]:
    """POST /ami/v1/outbound/dial and collect SSE events until terminal/EOF."""
    url = f"{http_url}/ami/v1/outbound/dial"
    events: list[dict] = []

    async with session.post(url, json=body, timeout=aiohttp.ClientTimeout(total=timeout)) as resp:
        if resp.status != 200:
            text = await resp.text()
            raise AssertionError(f"outbound/dial returned {resp.status}: {text}")

        event_name = ""
        data_buf = ""

        async for raw in resp.content:
            line = raw.decode("utf-8", errors="replace").rstrip()
            if line.startswith("event:"):
                event_name = line[len("event:"):].strip()
            elif line.startswith("data:"):
                data_buf += line[len("data:"):].strip()
            elif line == "":
                if event_name:
                    try:
                        payload = json.loads(data_buf) if data_buf else {}
                    except json.JSONDecodeError:
                        payload = {"raw": data_buf}
                    events.append({"event": event_name, "data": payload})
                    if event_name in _TERMINAL_EVENTS:
                        # Drain a few more events (e.g. call_bridged after
                        # call_answered) then stop.
                        async for extra in resp.content:
                            el = extra.decode("utf-8", errors="replace").rstrip()
                            if el.startswith("event:"):
                                event_name = el[len("event:"):].strip()
                            elif el.startswith("data:"):
                                data_buf = el[len("data:"):].strip()
                            elif el == "" and event_name:
                                try:
                                    payload = json.loads(data_buf) if data_buf else {}
                                except json.JSONDecodeError:
                                    payload = {"raw": data_buf}
                                events.append({"event": event_name, "data": payload})
                                event_name = ""
                                data_buf = ""
                        break
                event_name = ""
                data_buf = ""

    return events


def _event_names(events: list[dict]) -> list[str]:
    return [e["event"] for e in events]


async def _registered_callee(sipbot_pool, pbx, port, username="1002", **kw):
    defaults = dict(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", audio_quality=True,
    )
    defaults.update(kw)
    ua = sipbot_pool.callee(**defaults)
    await h.wait_registered(ua)
    return ua


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_outbound_sip_answer_success(pbx, sipbot_pool):
    """Originate to a sipbot callee that answers → RWI event sequence."""
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx)

    await _registered_callee(sipbot_pool, pbx, h.ua_port(15200), "1002")

    async with aiohttp.ClientSession() as session:
        events = await _sse_dial(session, pbx.http_url, {
            "call_id": f"ob-ok-{uuid.uuid4().hex[:8]}",
            "caller_id": f"sip:test@{pbx.sip_addr}",
            "destination": f"sip:1002@{pbx.sip_addr}",
            "ring_timeout": 10,
            "on_answer": {"type": "execute_flow"},
        }, timeout=30)

    names = _event_names(events)
    print("SSE events:", names)
    for e in events:
        print(f"  {e['event']}: {json.dumps(e['data'], ensure_ascii=False)[:200]}")

    assert "call_created" in names, f"missing call_created: {names}"
    assert "call_ringing" in names, f"missing call_ringing: {names}"
    assert "call_answered" in names, f"missing call_answered: {names}"


@pytest.mark.asyncio
async def test_outbound_sip_busy_failure(pbx, sipbot_pool):
    """Originate to a sipbot callee that rejects (486) → call_busy."""
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx)

    reject_ua = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(15201), username="1003", password="123456",
        ring_secs=2, reject_code=486, reject_prob=100,
    )
    await asyncio.sleep(1)

    async with aiohttp.ClientSession() as session:
        events = await _sse_dial(session, pbx.http_url, {
            "call_id": f"ob-busy-{uuid.uuid4().hex[:8]}",
            "caller_id": f"sip:test@{pbx.sip_addr}",
            "destination": f"sip:1003@127.0.0.1:{h.ua_port(15201)}",
            "ring_timeout": 10,
            "on_answer": {"type": "execute_flow"},
        }, timeout=30)

    print("sipbot output:", reject_ua.output[-500:])

    names = _event_names(events)
    print("SSE events:", names)

    assert "call_created" in names
    assert "call_busy" in names, f"expected call_busy, got: {names}"


@pytest.mark.asyncio
async def test_outbound_execute_flow(pbx, sipbot_pool):
    """on_answer = execute_flow → call_answered → stream closes."""
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx)

    await _registered_callee(sipbot_pool, pbx, h.ua_port(15202), "1002")

    async with aiohttp.ClientSession() as session:
        events = await _sse_dial(session, pbx.http_url, {
            "destination": f"sip:1002@{pbx.sip_addr}",
            "ring_timeout": 10,
            "on_answer": {"type": "execute_flow"},
        }, timeout=30)

    names = _event_names(events)
    print("SSE events:", names)

    assert "call_answered" in names


@pytest.mark.asyncio
async def test_outbound_app_after_answer(pbx, sipbot_pool):
    """on_answer = app(voicemail) → call_answered."""
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx)

    await _registered_callee(sipbot_pool, pbx, h.ua_port(15203), "1002")

    async with aiohttp.ClientSession() as session:
        events = await _sse_dial(session, pbx.http_url, {
            "destination": f"sip:1002@{pbx.sip_addr}",
            "ring_timeout": 10,
            "on_answer": {
                "type": "app",
                "app_name": "voicemail",
                "app_params": {},
            },
        }, timeout=30)

    names = _event_names(events)
    print("SSE events:", names)

    assert "call_answered" in names


@pytest.mark.asyncio
async def test_outbound_webhook_instruction(pbx, sipbot_pool):
    """on_answer = webhook → callee answers → sync webhook called."""
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx)

    await _registered_callee(sipbot_pool, pbx, h.ua_port(15204), "1002")

    from aiohttp import web

    webhook_called = asyncio.Event()

    async def handle(request):
        webhook_called.set()
        body = await request.json()
        print(f"webhook received: {json.dumps(body)[:300]}")
        return web.json_response({"action": "hangup"})

    app_web = web.Application()
    app_web.router.add_post("/handle", handle)
    runner = web.AppRunner(app_web)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    webhook_port = site._server.sockets[0].getsockname()[1]
    webhook_url = f"http://127.0.0.1:{webhook_port}/handle"

    try:
        async with aiohttp.ClientSession() as session:
            events = await _sse_dial(session, pbx.http_url, {
                "destination": f"sip:1002@{pbx.sip_addr}",
                "ring_timeout": 10,
                "on_answer": {
                    "type": "webhook",
                    "url": webhook_url,
                    "headers": {},
                    "timeout_secs": 5,
                    "fallback": {"type": "hangup"},
                },
            }, timeout=30)

        names = _event_names(events)
        print("SSE events:", names)
        for e in events:
            print(f"  {e['event']}: {json.dumps(e['data'], ensure_ascii=False)[:200]}")

        assert webhook_called.is_set(), "sync webhook was not called"
        assert "call_answered" in names
    finally:
        await runner.cleanup()


@pytest.mark.asyncio
async def test_outbound_no_answer_timeout(pbx, sipbot_pool):
    """Originate to a non-existent callee → call_no_answer / call_hangup."""
    pbx.config_builder.outbound_enabled = True
    h.boot_pbx(pbx)

    async with aiohttp.ClientSession() as session:
        events = await _sse_dial(session, pbx.http_url, {
            "destination": "sip:nobody@127.0.0.1:1",
            "ring_timeout": 3,
            "on_answer": {"type": "execute_flow"},
        }, timeout=20)

    names = _event_names(events)
    print("SSE events:", names)

    assert "call_created" in names
    assert (
        "call_no_answer" in names or "call_hangup" in names
    ), f"expected failure event, got: {names}"
