"""Step-provider (unified) IVR DTMF E2E: "press 2 → transfer" regression.

Reproduces the bug where a digit pressed while the current step is a
NON-interruptible prompt (e.g. a time announcement) was silently dropped by
`StepIvrApp::on_dtmf` ("ignoring early DTMF"), so the provider never received
the `dtmf` event and no transfer happened. The fix buffers the digit and
delivers it to the provider on the next step.

The route uses the exact wiring the unified IVR example documents:
`app = "ivr"`, `app_params = { mode = "step", url = "<mock>/ivr/step" }`.

- Test A: press 2 during the non-interruptible announcement -> the provider
  must receive `dtmf:2` and the call must transfer to the registered callee.
- Test B: press 2 during the interruptible welcome -> immediate barge-in
  transfer (guards the existing happy path).
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h

pytestmark = [pytest.mark.ivr]


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
        audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


def _start_step_provider(tmp_path):
    """Start a scripted step provider; returns (url, hits, cleanup).

    Provider contract:
      session_start   -> interruptible welcome prompt (1s)
      audio_complete  -> non-interruptible announcement prompt (3s)
      dtmf "2"        -> transfer to 1002
      anything else   -> hangup
    """
    from aiohttp import web
    from helpers import generate_sine_wav

    welcome = tmp_path / "welcome.wav"
    generate_sine_wav(welcome, 440.0, 1.0, 8000, 0.4)
    announce = tmp_path / "announce.wav"
    generate_sine_wav(announce, 660.0, 3.0, 8000, 0.4)

    hits: list[dict] = []

    async def handle_step(request: web.Request) -> web.Response:
        body = await request.json()
        hits.append(body)
        event = (body or {}).get("event") or {}
        ev_type = event.get("type")
        if ev_type == "session_start":
            return web.json_response(
                {"type": "prompt", "file": str(welcome), "interruptible": True}
            )
        if ev_type == "dtmf":
            digit = event.get("digit")
            if digit == "2":
                return web.json_response({"type": "transfer", "target": "1002"})
            return web.json_response({"type": "hangup"})
        if ev_type == "audio_complete":
            return web.json_response(
                {"type": "prompt", "file": str(announce), "interruptible": False}
            )
        return web.json_response({"type": "hangup"})

    app = web.Application()
    app.router.add_post("/ivr/step", handle_step)
    runner = web.AppRunner(app)

    async def start():
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        url = f"http://127.0.0.1:{site._server.sockets[0].getsockname()[1]}/ivr/step"
        return url

    async def cleanup():
        await runner.cleanup()

    return runner, hits, start, cleanup


def _add_step_route(cb, url: str):
    cb.add_route(
        "to-ivr-step",
        match={"to.user": "ivr-step"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"mode": "step", "url": url},
        auto_answer=True,
    )


def _has_dtmf(hits: list[dict], digit: str) -> bool:
    return any(
        (b.get("event") or {}).get("type") == "dtmf"
        and (b.get("event") or {}).get("digit") == digit
        for b in hits
    )


@pytest.mark.asyncio
async def test_step_ivr_dtmf_during_non_interruptible_prompt_transfers(pbx, sipbot_pool, tmp_path):
    """Press 2 during a NON-interruptible prompt must still transfer (bug fix)."""
    runner, hits, start, cleanup = _start_step_provider(tmp_path)
    try:
        url = await start()
        _add_step_route(pbx.config_builder, url)
        h.boot_pbx(pbx)

        callee = await _reg_callee(sipbot_pool, pbx, h.ua_port(15130), "1002")

        # welcome is 1s (interruptible), announce is 3s (non-interruptible).
        # Press 2 at 2s -> during the announcement.
        caller = sipbot_pool.caller(
            target=f"sip:ivr-step@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=12,
            dtmf_flows="2s:2",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

        # The buffered digit must reach the provider after the announcement and
        # transfer the call to 1002.
        assert await callee.wait_output_async(r"200 OK|Call established", timeout=20), (
            f"callee 1002 never received the transferred call:\n{callee.output[-1500:]}"
        )
        assert _has_dtmf(hits, "2"), (
            f"step provider never received dtmf:2 (the 'press 2' bug):\n{hits}"
        )
    finally:
        await cleanup()


@pytest.mark.asyncio
async def test_step_ivr_dtmf_during_interruptible_prompt_barges_in(pbx, sipbot_pool, tmp_path):
    """Press 2 during an interruptible prompt -> immediate barge-in transfer."""
    runner, hits, start, cleanup = _start_step_provider(tmp_path)
    try:
        url = await start()
        _add_step_route(pbx.config_builder, url)
        h.boot_pbx(pbx)

        callee = await _reg_callee(sipbot_pool, pbx, h.ua_port(15131), "1002")

        # welcome is interruptible (1s): press 2 at 1.5s during the welcome.
        caller = sipbot_pool.caller(
            target=f"sip:ivr-step@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=10,
            dtmf_flows="1.5s:2",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        assert await callee.wait_output_async(r"200 OK|Call established", timeout=20), (
            f"callee 1002 never received the transferred call:\n{callee.output[-1500:]}"
        )
        assert _has_dtmf(hits, "2"), f"step provider never received dtmf:2:\n{hits}"
    finally:
        await cleanup()
