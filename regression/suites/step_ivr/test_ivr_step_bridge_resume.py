"""Step-provider (unified) IVR bridge-resume E2E: the "menu replay" regression.

Reproduces the issue where a voip_bridge that ended WITHOUT buffered digits
(TTS prompt finished / remote close) made the resumed IVR instance open its
provider conversation with a SECOND `session_start`. Facades that key on
`session_start` semantics then re-entered the flow — the caller heard the
menu again ("多次 IVR").

The fix (ProviderEvent::Resume) makes the resumed instance open with
`{"type":"resume","resume_from_step_id":<suspended node>}`. This scenario
asserts the provider-side contract end to end:

- the bridge closes with no digits  ->  the next /ivr/step request carries
  `event.type == "resume"` with `resume_from_step_id` == the bridge step id,
  `variables.ivr_status == "resuming"`,
  `variables.ivr_resume_from_step_id` == the same node,
- and `session_start` was emitted EXACTLY ONCE for the whole call.

Provider contract:
  session_start  -> interruptible welcome prompt (1s)
  audio_complete -> voip_bridge to a local WS sink (no DTMF, closes itself)
  resume         -> hangup prompt (call ends)

If the fix regresses (a second session_start arrives instead of `resume`),
the provider falls through to the hangup default AND the assertions fail.
"""

from __future__ import annotations

import asyncio
import json

import pytest

import helpers as h

pytestmark = [pytest.mark.ivr]

BRIDGE_STEP_ID = "bridge-menu"


def _start_step_provider(tmp_path):
    """Start the scripted step provider + the bridge WS sink.

    Returns (runner, hits, bridge_hits, start, cleanup).
    """
    from aiohttp import WSMsgType, web

    from helpers import generate_sine_wav

    welcome = tmp_path / "welcome.wav"
    generate_sine_wav(welcome, 440.0, 1.0, 8000, 0.4)
    bye = tmp_path / "bye.wav"
    generate_sine_wav(bye, 550.0, 0.5, 8000, 0.4)

    hits: list[dict] = []
    bridge_hits: list[dict] = []

    async def handle_bridge(request: web.Request) -> web.WebSocketResponse:
        body = dict(request.query)
        body["connected"] = True
        bridge_hits.append(body)
        ws = web.WebSocketResponse()
        await ws.prepare(request)

        # Consume audio for a moment, then close WITHOUT any DTMF — the
        # "TTS prompt finished naturally" case. `asyncio.wait` returns on
        # timeout without raising, so no exception is swallowed here.
        received = {"frames": 0}

        async def _drain() -> None:
            async for msg in ws:
                if msg.type == WSMsgType.BINARY:
                    received["frames"] += 1

        drain_task = asyncio.ensure_future(_drain())
        await asyncio.wait([drain_task], timeout=1.5)
        drain_task.cancel()
        await ws.close()
        body["audio_frames"] = received["frames"]
        bridge_hits.append(body)
        return ws

    async def handle_step(request: web.Request) -> web.Response:
        body = await request.json()
        hits.append(body)
        event = (body or {}).get("event") or {}
        ev_type = event.get("type")
        if ev_type == "session_start":
            return web.json_response(
                {"type": "prompt", "file": str(welcome), "interruptible": True}
            )
        if ev_type == "audio_complete":
            return web.json_response(
                {
                    "type": "voip_bridge",
                    "create_room_uri": BRIDGE_WS_URL[0],
                    "timeout_ms": 10000,
                    "step_id": BRIDGE_STEP_ID,
                    "step_name": "menu bridge",
                    "return_app": "ivr",
                    "return_target": "resume-ivr",
                }
            )
        if ev_type == "resume":
            return web.json_response(
                {"type": "hangup", "prompt": str(bye)}
            )
        return web.json_response({"type": "hangup"})

    app = web.Application()
    app.router.add_post("/ivr/step", handle_step)
    app.router.add_get("/ivr/bridge", handle_bridge)
    runner = web.AppRunner(app)

    BRIDGE_WS_URL = [None]

    async def start():
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        host, port = site._server.sockets[0].getsockname()[:2]
        BRIDGE_WS_URL[0] = f"ws://127.0.0.1:{port}/ivr/bridge"
        return f"http://127.0.0.1:{port}/ivr/step"

    async def cleanup():
        await runner.cleanup()

    return runner, hits, bridge_hits, start, cleanup


def _step_ivr_toml(name: str, provider_url: str) -> str:
    return f"""\
[ivr]
name = "{name}"
ivr_mode = "step"

[ivr.provider]
url = "{provider_url}"
max_retries = 2
retry_delay_ms = 500
timeout_secs = 5
"""


def _add_step_route(cb, url: str):
    # Entry via the IVR FILE (not inline mode/url): the executor then
    # remembers the start file, so the bridge return (`return_target:
    # resume-ivr`) resolves the same step IVR — the production wiring.
    cb.add_ivr("resume-ivr", _step_ivr_toml("resume-ivr", url))
    cb.add_route(
        "to-ivr-step",
        match={"to.user": "ivr-step"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/resume-ivr.toml"},
        auto_answer=True,
    )


def _resume_hits(hits: list[dict]) -> list[dict]:
    return [b for b in hits if (b.get("event") or {}).get("type") == "resume"]


def _session_start_count(hits: list[dict]) -> int:
    return sum(
        1 for b in hits if (b.get("event") or {}).get("type") == "session_start"
    )


@pytest.mark.asyncio
async def test_step_ivr_bridge_close_without_digits_sends_resume_not_session_start(
    pbx, sipbot_pool, tmp_path
):
    """Bridge ended with no digits -> provider sees `resume`, never a second session_start."""
    runner, hits, bridge_hits, start, cleanup = _start_step_provider(tmp_path)
    try:
        url = await start()
        _add_step_route(pbx.config_builder, url)
        h.boot_pbx(pbx)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-step@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=12,
        )
        assert await caller.wait_output_async(
            r"200 OK|Call established", timeout=25
        ), caller.output

        # The bridge sink closes itself ~1.5s after the bridge is
        # established; rustpbx must then resume the flow. Wait until the
        # provider logged the resume round-trip (or the call ended).
        for _ in range(60):
            if _resume_hits(hits):
                break
            await asyncio.sleep(0.25)
        await asyncio.sleep(0.5)

        # ── The contract under test ──────────────────────────────────────
        resumes = _resume_hits(hits)
        assert resumes, (
            "provider never received a `resume` event after the bridge "
            f"closed; requests were:\n{json.dumps(hits, ensure_ascii=False, indent=2)}"
        )
        assert _session_start_count(hits) == 1, (
            "session_start must be emitted exactly once per logical flow — "
            f"a second one is the menu-replay regression:\n{hits}"
        )
        resume_event = resumes[0].get("event") or {}
        assert resume_event.get("resume_from_step_id") == BRIDGE_STEP_ID, (
            f"resume must carry the bridge suspension point: {resume_event}"
        )
        variables = resumes[0].get("variables") or {}
        assert variables.get("ivr_status") == "resuming", (
            f"resumed instance must advertise ivr_status=resuming: {variables}"
        )
        assert variables.get("ivr_resume_from_step_id") == BRIDGE_STEP_ID, (
            f"variables must expose the suspension point: {variables}"
        )
        assert bridge_hits, "rustpbx never connected to the bridge WS sink"
    finally:
        await cleanup()
