"""Supervisor monitor E2E — listen / whisper / barge over real SIP.

`supervisor.listen` attaches a supervisor leg (an originated call) to a live
target call via the conference-bridge path. The monitor events are
Owner-dispatched, so they are observed on the webhook bus.

Known gap (strict xfail): the monitor AUDIO (supervisor hearing the parties)
rides the same conference-media reverse path that is broken for dial-in
rooms (see test_conference.py) — the supervisor leg receives mixer output
but no target audio is mixed in.
"""

from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.media]


def _cid(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


async def _reg(sipbot_pool, pbx, port: int, username: str, **kw):
    ua = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", **kw,
    )
    await h.wait_registered(ua)
    return ua


async def _find_session_call(rwi, caller: str, callee: str,
                             timeout: float = 15) -> str:
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        resp = await rwi.list_calls()
        data = (resp.get("data") or [])
        if isinstance(data, dict):
            data = data.get("calls") or []
        for entry in data:
            blob = str(entry)
            if caller in blob and callee in blob:
                cid = entry.get("call_id") or entry.get("session_id") or entry.get("id") or ""
                if cid:
                    return cid
        await asyncio.sleep(0.3)
    raise AssertionError(f"no live call {caller}→{callee} in list_calls: {resp}")


async def _wait_caller_audio(ua, label: str, min_frames: int = 50, timeout: float = 15):
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        aq = ua.get_audio_quality()
        if aq and aq.get("total_frames", 0) >= min_frames:
            return
        await asyncio.sleep(0.5)
    raise AssertionError(f"{label}: no audio frames: {ua.get_audio_quality()}")


async def _setup_call(pbx, sipbot_pool, rwi):
    await h.connect_rwi(rwi)
    await _reg(sipbot_pool, pbx, h.ua_port(15504), "1002")
    await _reg(sipbot_pool, pbx, h.ua_port(15505), "1003", audio_quality=True)
    caller = sipbot_pool.caller(
        target=f"sip:1002@{pbx.sip_addr}", username="1001", password="123456",
        hangup=30, audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=20), (
        caller.output
    )
    await _wait_caller_audio(caller, "A")
    session_id = await _find_session_call(rwi, "1001", "1002")

    sup_call = _cid("sup")
    r = await rwi.originate(sup_call, f"sip:1003@{pbx.sip_addr}", "sip:sup@pbx", "default")
    assert r.get("status") == "success", r
    await asyncio.sleep(1.5)
    return caller, session_id, sup_call


@pytest.mark.asyncio
async def test_supervisor_mode_lifecycle(pbx, sipbot_pool, rwi,
                                         webhook_server, webhook_session):
    """listen → whisper → barge → stop: every monitor mode starts (webhook
    event) and the original call survives the whole supervision cycle."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    caller, session_id, sup_call = await _setup_call(pbx, sipbot_pool, rwi)

    rl = await rwi.supervisor_listen(sup_call, session_id)
    assert rl.get("status") == "success", rl
    assert await webhook_session.wait_for_event(
        "supervisor_listen_started", timeout=10
    ), f"no listen event: {webhook_session.event_types()[-15:]}"

    rw = await rwi.supervisor_whisper(sup_call, session_id, agent_leg="")
    assert rw.get("status") == "success", rw
    assert await webhook_session.wait_for_event(
        "supervisor_whisper_started", timeout=10
    ), f"no whisper event: {webhook_session.event_types()[-15:]}"

    rb = await rwi.supervisor_barge(sup_call, session_id, agent_leg="")
    assert rb.get("status") == "success", rb
    assert await webhook_session.wait_for_event(
        "supervisor_barge_started", timeout=10
    ), f"no barge event: {webhook_session.event_types()[-15:]}"

    rs = await rwi.supervisor_stop(sup_call, session_id)
    assert rs.get("status") == "success", rs
    assert await webhook_session.wait_for_event(
        "supervisor_mode_stopped", timeout=10
    ), f"no stop event: {webhook_session.event_types()[-15:]}"

    # Original call still alive and media-bearing after the whole cycle.
    assert caller.is_alive, "original call died during supervision"
    await rwi.hangup(session_id)


@pytest.mark.asyncio
async def test_supervisor_takeover_kicks_agent_keeps_customer(
    pbx, sipbot_pool, rwi, webhook_server, webhook_session
):
    """Takeover (强拆): the agent leg is kicked (real SIP BYE) while the
    customer and supervisor legs survive in the takeover conference."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    caller, session_id, sup_call = await _setup_call(pbx, sipbot_pool, rwi)

    def _bot(name: str):
        for p in sipbot_pool._procs:
            if p.name == name:
                return p
        raise AssertionError(f"sipbot {name} not found")

    agent_bot = _bot("1002")
    sup_bot = _bot("1003")

    rt = await rwi.supervisor_takeover(sup_call, session_id)
    assert rt.get("status") == "success", rt
    assert await webhook_session.wait_for_event(
        "supervisor_takeover_started", timeout=10
    ), f"no takeover event: {webhook_session.event_types()[-15:]}"

    # The target session actually executed the cross-session takeover.
    await h.wait_log(
        pbx, r"Supervisor takeover \(cross-session\) activated",
        timeout=15, label="takeover activated",
    )

    # The agent leg is kicked: the agent UA receives a BYE.
    assert await agent_bot.wait_output_async(
        r"BYE|bye|hangup|terminated|Call ended", timeout=15
    ), f"agent 1002 was not kicked by takeover:\n{agent_bot.output[-1500:]}"

    # Customer and supervisor legs survive the takeover.
    await asyncio.sleep(2)
    assert caller.is_alive, "customer call died during takeover"
    assert sup_bot.is_alive, "supervisor call died during takeover"

    await rwi.hangup(session_id)


@pytest.mark.asyncio
@pytest.mark.xfail(
    reason="supervisor monitor audio rides the conference-media reverse path, "
           "which delivers silence (same root cause as the conference dial-in "
           "mixing xfail in test_conference.py: UA→mixer input not wired).",
    strict=True,
)
async def test_supervisor_listen_hears_target_audio(pbx, sipbot_pool, rwi,
                                                    webhook_server, webhook_session):
    """While listening, the supervisor UA must receive the monitored call's
    audio (real RTP content, not just bridge silence)."""
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    caller, session_id, sup_call = await _setup_call(pbx, sipbot_pool, rwi)

    rl = await rwi.supervisor_listen(sup_call, session_id)
    assert rl.get("status") == "success", rl
    assert await webhook_session.wait_for_event(
        "supervisor_listen_started", timeout=10
    ), webhook_session.event_types()[-15:]

    # The supervisor UA (wait-mode bot on 15505) must hear non-silent audio.
    sup_bot = None
    for p in sipbot_pool._procs:
        if p.name == "1003":
            sup_bot = p
            break
    assert sup_bot is not None
    deadline = asyncio.get_event_loop().time() + 15
    ok = False
    while asyncio.get_event_loop().time() < deadline:
        aq = sup_bot.get_audio_quality()
        if aq and aq.get("has_audio"):
            ok = True
            break
        await asyncio.sleep(0.5)
    assert ok, f"supervisor heard no target audio: {sup_bot.get_audio_quality()}"
