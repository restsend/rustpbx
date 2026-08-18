"""Step-mode IVR action E2E tests.

These complement test_ivr_step_dtmf.py (prompt/transfer/hangup/dtmf_menu/bridge)
by exercising the remaining step-mode-only actions end-to-end via a scripted
HTTP provider:

  - api           : provider returns an `api` node -> PBX fires HTTP to the URL
  - route_to_agent: provider returns route_to_agent -> registered agent answers
  - queue         : provider returns queue -> queue dials agent -> agent answers
  - input_phone   : provider returns input_phone -> digits collected -> transfer
  - jump_ivr      : provider returns jump_ivr -> target tree IVR runs -> transfers

Each test wires `app = "ivr"` with `app_params = {mode = "step", url = ...}`
(the unified step wiring) and asserts a real SIP-observable outcome.
"""

from __future__ import annotations

import asyncio

import pytest

import helpers as h

pytestmark = [pytest.mark.ivr]


def _start_step_provider(handler):
    """Start a scripted step-mode provider on an ephemeral port.

    ``handler(body) -> dict`` maps each provider POST to an ActionNode JSON
    response. Returns ``(runner, hits, start, cleanup)`` where ``start`` is an
    async fn returning the provider URL and ``hits`` collects every request body.
    """
    from aiohttp import web

    hits: list[dict] = []

    async def handle(request):
        body = await request.json()
        hits.append(body)
        return web.json_response(handler(body))

    app = web.Application()
    app.router.add_post("/ivr/step", handle)
    runner = web.AppRunner(app)

    async def start():
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        return f"http://127.0.0.1:{port}/ivr/step"

    async def cleanup():
        await runner.cleanup()

    return runner, hits, start, cleanup


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
        audio_quality=True,
    )
    await h.wait_registered(ua)
    return ua


# ---------------------------------------------------------------------------
# api
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_api_action_fires_http(pbx, sipbot_pool, tmp_path):
    """`api` action (step mode) — PBX must call the action's HTTP URL."""
    from aiohttp import web

    api_hits: list[dict] = []

    async def _start_api():
        async def handle(request):
            api_hits.append({"method": request.method})
            return web.json_response({"ok": True})

        app = web.Application()
        app.router.add_get("/", handle)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]

        async def cleanup():
            await runner.cleanup()

        return f"http://127.0.0.1:{port}/", cleanup

    api_url, api_cleanup = await _start_api()

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "api", "url": api_url, "method": "GET", "timeout": 5}
        return {"type": "hangup"}

    _runner, _hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-api", "ivr-api", url)
        h.boot_pbx(pbx)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-api@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=8,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        # The Api action must fire its HTTP request to the mock endpoint.
        await asyncio.sleep(3)
        assert api_hits, f"api action did not reach its URL: {api_hits}"
    finally:
        await cleanup()
        await api_cleanup()


# ---------------------------------------------------------------------------
# route_to_agent
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_route_to_agent_transfers(pbx, sipbot_pool, tmp_path):
    """`route_to_agent` action (step mode) — registered agent receives the call."""
    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {
                "type": "route_to_agent",
                "target": "1002",
                "skill_group_id": "support",
                "channel_code": "web",
            }
        return {"type": "hangup"}

    _runner, _hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-rta", "ivr-rta", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, h.ua_port(15140), "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-rta@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=14,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"agent 1002 never received the routed call:\n{agent.output[-1500:]}"
        )
    finally:
        await cleanup()


# ---------------------------------------------------------------------------
# queue
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_queue_action_routes_to_agent(pbx, sipbot_pool, tmp_path):
    """`queue` action (step mode) — queue dials the agent which answers."""
    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "queue", "target": "support"}
        return {"type": "hangup"}

    _runner, _hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        pbx.config_builder.add_queue(
            "support",
            strategy_mode="sequential",
            targets=[f"sip:1002@127.0.0.1:{h.ua_port(15141)}"],
            accept_immediately=True,
            wait_timeout_secs=15,
        )
        _add_step_route(pbx, "to-ivr-queue", "ivr-q", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, h.ua_port(15141), "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-q@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=20,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=30), (
            f"agent 1002 never received the queued call:\n{agent.output[-1500:]}"
        )
        # Media bridge must be active: the agent must receive RTP from the
        # caller. A `has_rx or has_tx` check on the caller would pass on
        # hold-music RTP even if the caller↔agent bridge never activated.
        await h.wait_rtp_rx(agent, "agent", 25)
    finally:
        await cleanup()


# ---------------------------------------------------------------------------
# input_phone
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_input_phone_collects_and_transfers(pbx, sipbot_pool, tmp_path):
    """`input_phone` action (step mode) — collects digits then provider transfers."""
    from helpers import generate_sine_wav

    prompt = tmp_path / "enter_phone.wav"
    generate_sine_wav(prompt, 440.0, 1.0, 8000, 0.4)

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {
                "type": "input_phone",
                "prompt": str(prompt),
                "min_digits": 11,
                "max_digits": 11,
            }
        if ev.get("type") == "phone_collected":
            return {"type": "transfer", "target": "1002"}
        return {"type": "hangup"}

    _runner, _hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-phone", "ivr-phone", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, 15142, "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-phone@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=20,
            dtmf_flows="3s:12345678901#",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=30), (
            f"agent 1002 never received the transferred call:\n{agent.output[-1500:]}"
        )
    finally:
        await cleanup()


# ---------------------------------------------------------------------------
# jump_ivr
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_jump_ivr_runs_target(pbx, sipbot_pool, tmp_path):
    """`jump_ivr` action (step mode) — jumps to a target tree IVR which transfers."""
    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "jump_ivr", "route_point": "ivr-target"}
        return {"type": "hangup"}

    _runner, _hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        # Target tree IVR: empty greeting -> immediate DTMF wait -> timeout ->
        # max_retries=0 fires max_retries_action (transfer to 1002).
        pbx.config_builder.add_ivr(
            "ivr-target",
            """\
[ivr]
name = "ivr-target"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = ""
timeout_ms = 1000
max_retries = 0
max_retries_action = { type = "transfer", target = "1002" }
entries = []
""",
        )
        _add_step_route(pbx, "to-ivr-jump", "ivr-jump", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, 15143, "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-jump@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=22,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        # jump_ivr -> ivr-target -> timeout -> transfer to 1002.
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=30), (
            f"agent 1002 never received the jumped call:\n{agent.output[-1500:]}"
        )
    finally:
        await cleanup()


# ---------------------------------------------------------------------------
# wait_for_result: application-owned (awaited) transfers
# ---------------------------------------------------------------------------

def _find_transfer_result(hits: list[dict]) -> dict:
    """Return the latest `transfer_result` event seen by a step provider."""
    result: dict = {}
    for body in hits:
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "transfer_result":
            result = ev
    return result


async def _wait_transfer_result(hits: list[dict], timeout: float = 25) -> dict:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        ev = _find_transfer_result(hits)
        if ev:
            return ev
        await asyncio.sleep(0.5)
    raise AssertionError(
        f"provider never received transfer_result; events: "
        f"{[ (b or {}).get('event') for b in hits ]}"
    )


def _assert_caller_hears_tone(caller_rec: Path, target_hz: float) -> None:
    """Best-effort: the caller recording must contain a run of `target_hz`."""
    from helpers import read_wav_mono, find_dominant_frequency, compute_rms_db

    if not caller_rec.exists() or caller_rec.stat().st_size < 1000:
        pytest.skip("caller recording too small or missing")

    rx, sr = read_wav_mono(caller_rec)
    rx = rx.ravel()
    W = sr // 50  # 20 ms window
    tone_windows = 0
    for i in range(0, len(rx) - W, W):
        chunk = rx[i:i + W]
        if compute_rms_db(chunk) > -40.0:
            f, _mag = find_dominant_frequency(chunk, sr, 200, 450, 5)
            if abs(f - target_hz) < 50.0:
                tone_windows += 1
    assert tone_windows >= 5, (
        f"caller never heard the {target_hz:.0f} Hz post-transfer prompt "
        f"({tone_windows} windows) — the IVR may have lost media control"
    )


@pytest.mark.asyncio
async def test_step_ivr_await_result_target_ended_returns_to_app(pbx, sipbot_pool, tmp_path):
    """`transfer` with `wait_for_result=true`: the target answers then hangs up.

    The provider must receive a typed `transfer_result` event with outcome
    `target_ended`, the caller must survive the target hangup (returned to the
    IVR app, NOT hung up with it), and the IVR must regain media control —
    playing a fresh post-transfer prompt the caller actually hears.
    """
    from helpers import generate_sine_wav

    after = tmp_path / "await_after.wav"
    generate_sine_wav(after, 330.0, 2.0, 8000, 0.5)
    caller_rec = tmp_path / "await_caller.wav"

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "transfer", "target": "1002", "wait_for_result": True}
        if ev.get("type") == "transfer_result":
            return {"type": "prompt", "file": str(after), "interruptible": False}
        if ev.get("type") == "audio_complete":
            return {"type": "hangup"}
        return {"type": "hangup"}

    _runner, hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-await", "ivr-await", url)
        h.boot_pbx(pbx)

        agent = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(15144), username="1002", password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="echo", hangup_after=5,
        )
        await h.wait_registered(agent)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-await@{pbx.sip_addr}", username="1001", password="123456",
            hangup=20, record_file=str(caller_rec), audio_quality=True,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
        assert await agent.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"agent 1002 never received the awaited transfer:\n{agent.output[-1500:]}"
        )

        # Agent hangs up after ~5 s → provider must get target_ended.
        outcome = await _wait_transfer_result(hits)
        assert outcome.get("outcome") == "target_ended", f"unexpected outcome: {outcome}"

        # The caller must NOT have been hung up along with the target — the
        # app owns the call again and now plays the post-transfer prompt.
        assert caller.is_alive, (
            f"caller hung up when the transfer target ended:\n{caller.output[-1500:]}"
        )
        caller.wait(timeout=25)
    finally:
        await cleanup()

    _assert_caller_hears_tone(caller_rec, 330.0)


@pytest.mark.asyncio
async def test_step_ivr_await_result_not_connected(pbx, sipbot_pool, tmp_path):
    """`transfer` with `wait_for_result=true`: the target rejects (486 Busy).

    The provider must receive outcome `not_connected`, the caller must NOT be
    hung up, and the IVR must continue normally — playing a fresh prompt the
    caller actually hears — before the provider hangs the call up.
    """
    from helpers import generate_sine_wav

    after = tmp_path / "nc_after.wav"
    generate_sine_wav(after, 330.0, 2.0, 8000, 0.5)
    caller_rec = tmp_path / "nc_caller.wav"

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {"type": "transfer", "target": "1002", "wait_for_result": True}
        if ev.get("type") == "transfer_result":
            return {"type": "prompt", "file": str(after), "interruptible": False}
        if ev.get("type") == "audio_complete":
            return {"type": "hangup"}
        return {"type": "hangup"}

    _runner, hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-nc", "ivr-nc", url)
        h.boot_pbx(pbx)

        agent = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(15145), username="1002", password="123456",
            register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=1, answer_mode="none", reject_code=486, reject_prob=100,
        )
        await h.wait_registered(agent)

        caller = sipbot_pool.caller(
            target=f"sip:ivr-nc@{pbx.sip_addr}", username="1001", password="123456",
            hangup=15, record_file=str(caller_rec), audio_quality=True,
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

        outcome = await _wait_transfer_result(hits)
        assert outcome.get("outcome") == "not_connected", f"unexpected outcome: {outcome}"

        # The caller was never handed off and must still be alive while the IVR
        # plays the post-transfer prompt.
        assert caller.is_alive, (
            f"caller hung up after a not_connected awaited transfer:\n{caller.output[-1500:]}"
        )
        caller.wait(timeout=25)
    finally:
        await cleanup()

    _assert_caller_hears_tone(caller_rec, 330.0)


# ---------------------------------------------------------------------------
# input_phone: validated timing options
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_step_ivr_input_phone_custom_terminator(pbx, sipbot_pool, tmp_path):
    """`input_phone` honors a custom `terminator` (here `*` instead of `#`).

    The caller presses `7` then `*`; the `*` must end collection so the
    provider receives `phone_collected` with number `7`.
    """
    from helpers import generate_sine_wav

    prompt = tmp_path / "term_prompt.wav"
    generate_sine_wav(prompt, 440.0, 1.0, 8000, 0.4)

    collected: list[str] = []

    def handler(body):
        ev = (body or {}).get("event") or {}
        if ev.get("type") == "session_start":
            return {
                "type": "input_phone",
                "prompt": str(prompt),
                "min_digits": 1,
                "max_digits": 11,
                "terminator": "*",
            }
        if ev.get("type") == "phone_collected":
            collected.append(ev.get("number", ""))
            return {"type": "transfer", "target": "1002"}
        return {"type": "hangup"}

    _runner, hits, start, cleanup = _start_step_provider(handler)
    try:
        url = await start()
        _add_step_route(pbx, "to-ivr-term", "ivr-term", url)
        h.boot_pbx(pbx)

        agent = await _reg_callee(sipbot_pool, pbx, 15146, "1002")
        caller = sipbot_pool.caller(
            target=f"sip:ivr-term@{pbx.sip_addr}",
            username="1001",
            password="123456",
            hangup=15,
            dtmf_flows="3s:7*",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

        deadline = asyncio.get_event_loop().time() + 15
        while asyncio.get_event_loop().time() < deadline and not collected:
            await asyncio.sleep(0.5)
        assert collected, (
            f"provider never received phone_collected; events: "
            f"{[ (b or {}).get('event') for b in hits ]}"
        )
        assert collected == ["7"], f"expected '7', got {collected}"

        assert await agent.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"agent 1002 never received the transferred call:\n{agent.output[-1500:]}"
        )
    finally:
        await cleanup()
