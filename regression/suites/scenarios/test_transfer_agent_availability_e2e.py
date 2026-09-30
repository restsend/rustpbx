"""Agent availability & consult-family e2e — restsend-cli + sipbot endpoints.

G — agent repeatedly unavailable / retry chains:
  G1  blind REFER → CC-busy agent (486) → retry → success
  G2a blind REFER → queue, agent on BREAK → recovers → dispatched
  G2b blind REFER → queue, agent stays on BREAK → wait timeout → 486 fallback
  G3  queue ring-timeout rotation: 2 never-answer agents → 3rd answers
  G4  consult → busy target fails → main recovered → re-consult → retrieve

H — consult-family combinations (audio-verified):
  H5  consult/retrieve cycle ×2 → attended transfer (per-cycle tone timeline)
  H6  consult target BYE mid-consult → auto-resume → re-consult
  H7  switch away → held consult leg BYE → customer keeps talking
  H8  3-way conference mixing matrix → one party leaves → correct fall-back

AUDIO (spectral): 1001 → 620 Hz, 1002 → 920 Hz (RESTSEND_TONE_HZ),
1015 → 730 Hz — all outside the MOH band; `band_gain_db ≥ -12` = present,
`≤ -20` = absent; aligned via the 920 Hz marker (`rec_align_offset`).
"""

from __future__ import annotations

import asyncio
import os
import time
from pathlib import Path

import pytest

import conftest as root_conftest
import helpers as h
from helpers import (
    band_gain_db,
    generate_sine_wav,
    read_wav_mono,
    rec_align_offset,
    wait_recording_async,
)
from helpers.pbx_server import PbxServer
from helpers.restsend_agent import CLI as RESTSEND_CLI
from helpers.restsend_agent import RestsendAgent

pytestmark = [pytest.mark.scenario]

CUSTOMER = "1001"   # sipbot caller (620 Hz)
AGENT = "1002"      # restsend-cli transferor (920 Hz tone)
TARGET = "1003"     # restsend-cli capability target (busy-able, BYE on demand)
BUSY_PEER = "1004"  # restsend-cli — keeps 1003 busy
ECHO = "1014"       # sipbot echo callee (retry/re-consult target)
TONE730 = "1015"    # sipbot callee playing 730 Hz (spectral target)
BREAK_AGENT = "1016"  # restsend-cli CC agent on the av queue
ROT_A = "1017"      # restsend-cli never-answer (rot queue)
ROT_B = "1018"      # restsend-cli never-answer (rot queue)
ROT_OK = "1019"     # sipbot echo (rot queue, answers)

AV_NUM = "9202"
ROT_NUM = "9203"
SKILL_AV = "av"
SKILL_ROT = "rot"

F_AGENT = 920.0
F_CUST = 620.0
F_TARGET = 730.0


def _skip_without_cli() -> None:
    if not os.path.exists(RESTSEND_CLI):
        pytest.skip(
            f"restsend-cli not built: {RESTSEND_CLI} "
            "(cargo build -p restsend-cli in ../restsend-call)"
        )


# ---------------------------------------------------------------------------
# module-scoped PBX
# ---------------------------------------------------------------------------

@pytest.fixture(scope="module")
def avail_pbx(webhook_server) -> PbxServer:
    server = PbxServer(
        host=root_conftest.SIP_HOST,
        sip_port=root_conftest.SIP_PORT,
        http_port=root_conftest.HTTP_PORT,
        rwi_token=root_conftest.RWI_TOKEN,
        project_root=root_conftest.PROJECT_ROOT,
        work_dir=root_conftest.ARTIFACT_ROOT,
    )
    cb = server.config_builder
    cb.add_memory_users([BUSY_PEER, ECHO, TONE730, "1016", "1017", "1018",
                         "1019", "1051", "1052", "1053", "1054"])
    cb.add_queue(
        "av", strategy_mode="sequential",
        targets=[f"skill-group:{SKILL_AV}"],
        ring_timeout_secs=6, wait_timeout_secs=20, fallback_failure_code=486,
    )
    cb.add_route(
        "av-entry-route", match={"to.user": AV_NUM}, priority=10,
        action="queue", queue="av", auto_answer=True,
    )
    cb.add_queue(
        "rot", strategy_mode="sequential",
        targets=[f"skill-group:{SKILL_ROT}"],
        ring_timeout_secs=6, wait_timeout_secs=60,
    )
    cb.add_route(
        "rot-entry-route", match={"to.user": ROT_NUM}, priority=10,
        action="queue", queue="rot", auto_answer=True,
    )
    cb.set_proxy_extra(
        conference_factory_uri=f"sip:conf-factory@{root_conftest.SIP_HOST}:{root_conftest.SIP_PORT}")
    server.prepare(webhook_url=webhook_server.url, build=False)
    server.start(timeout=90)
    _seed_cc_sync(server)
    yield server
    server.stop()


def _seed_cc_sync(pbx) -> None:
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    async def _run():
        session = aiohttp.ClientSession()
        try:
            client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
            assert await client.ensure_console_auth(), "console auth failed"
            for body in (
                {"agent_id": BREAK_AGENT, "display_name": "Agent 1016 (av)",
                 "skills": [SKILL_AV], "max_concurrency": 3, "role": "agent"},
                {"agent_id": ROT_A, "display_name": "Agent 1017 (rot)",
                 "skills": [SKILL_ROT], "max_concurrency": 3, "role": "agent"},
                {"agent_id": ROT_B, "display_name": "Agent 1018 (rot)",
                 "skills": [SKILL_ROT], "max_concurrency": 3, "role": "agent"},
                {"agent_id": ROT_OK, "display_name": "Agent 1019 (rot)",
                 "skills": [SKILL_ROT], "max_concurrency": 3, "role": "agent"},
                {"skill_group_id": SKILL_AV, "skills_required": [SKILL_AV],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 20,
                 "metadata": {"wrapup_time_secs": 2}},
                {"skill_group_id": SKILL_ROT, "skills_required": [SKILL_ROT],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 120,
                 "metadata": {"wrapup_time_secs": 2}},
            ):
                try:
                    if "skill_group_id" in body:
                        await client.create_skill_group(body)
                    else:
                        await client.create_agent(body)
                except Exception as exc:  # noqa: BLE001 — duplicates fine
                    if not ("409" in str(exc) or "400" in str(exc)
                            or "already" in str(exc).lower()):
                        raise
        finally:
            await session.close()

    asyncio.run(_run())


# ---------------------------------------------------------------------------
# harness
# ---------------------------------------------------------------------------

def _restsend(pbx, user: str, port: int, device: str = "tone",
              tone_hz: int | None = 920) -> RestsendAgent:
    return RestsendAgent(
        pbx, user, local_port=h.ua_port(port), device=device,
        tone_hz=tone_hz,
        log_level=os.environ.get("RESTSEND_E2E_LOG", "info"))


def _tone_target(sipbot_pool, pbx, port: int, username: str, tone: Path,
                 **kw):
    """sipbot callee that answers playing a tone file (its TX), recording RX."""
    kw.setdefault("audio_quality", True)
    bot = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode=str(tone), **kw,
    )
    assert bot, f"{username} bot failed to start"
    return bot


def _echo_bot(sipbot_pool, pbx, port: int, username: str, **kw):
    kw.setdefault("audio_quality", True)
    bot = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", **kw,
    )
    assert bot
    return bot


def _ringing_restsend(pbx, user: str, port: int) -> RestsendAgent:
    """A restsend phone that registers and never answers anything."""
    return _restsend(pbx, user, port, device="null")


def _customer(sipbot_pool, pbx, tmp_path: Path, username=CUSTOMER,
              target=None, port_base=20400, hangup=150):
    tone = tmp_path / f"cust_{username}_620.wav"
    generate_sine_wav(tone, 620.0, 150.0, 8000, 0.4)
    rec = tmp_path / f"cust_{username}_rx.wav"
    bot = sipbot_pool.caller(
        target=target or f"sip:{AGENT}@{pbx.sip_addr}",
        username=username, password="123456",
        proxy=f"{pbx.host}:{pbx.sip_port}",
        hangup=hangup, play_file=str(tone), record_file=str(rec),
        audio_quality=True, addr=f"127.0.0.1:{h.ua_port(port_base)}",
    )
    assert bot, f"{username} caller failed to start"
    return bot


async def _establish(pbx, sipbot_pool, tmp_path: Path, *, customer=CUSTOMER,
                     port_base=20400, agent=AGENT, agent_port=25110):
    """Agent registers first, then the customer dials in; returns
    (agent, customer_bot, t_est) — t_est = time.monotonic() at media flow."""
    agent = _restsend(pbx, agent, agent_port)
    await agent.start()
    assert await agent.register(expires=120), f"{agent} REGISTER failed"
    cust = _customer(sipbot_pool, pbx, tmp_path, username=customer,
                     port_base=port_base)
    assert await agent.answer(timeout=25), (
        f"{agent} did not ring/answer:\n{agent.stderr_text()[-600:]}")
    await agent.wait_media_flow(min_received=10, timeout=10, label="establish")
    return agent, cust, time.monotonic()


async def _pair_up(a: RestsendAgent, b: RestsendAgent, pbx) -> None:
    """a calls b, b answers — leaves `a` BUSY (its phone rejects further
    INVITEs with 486)."""
    await a.cmd({"cmd": "sip_call",
                 "remote_uri": f"sip:{b.user}@{pbx.sip_addr}"})
    assert await b.answer(timeout=15), (
        f"{b.user} never answered the busy-pair call:\n{b.stderr_text()[-400:]}")
    assert await a.wait_state("connected", timeout=10), (
        f"{a.user} busy-pair never connected")


async def _drain(api, timeout: float = 30.0) -> None:
    """Repeatedly force-end sessions until the registry is empty (a queue
    session may need a second /end while its app unwinds)."""
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while True:
        await _end_all(api)
        if not await _active_calls(api):
            return
        if loop.time() >= deadline:
            break
        await asyncio.sleep(1.5)
    raise AssertionError(f"sessions still active: {await _active_calls(api)}")


async def _consult_with_answer(agent: RestsendAgent, partner: RestsendAgent,
                               uri: str) -> Optional[dict]:
    """Consult `uri` where the target is a restsend phone that must ANSWER
    explicitly (the agent-side `consult()` helper would block waiting for a
    connected leg nobody answers)."""
    mark = len(agent.events)
    await agent.cmd({"cmd": "sip_call", "remote_uri": uri, "consult": True})
    assert await partner.answer(timeout=15), (
        f"{partner.user} never answered the consult:\n"
        f"{partner.stderr_text()[-400:]}")
    return await agent.wait_event_after(
        "sip_call_leg", mark,
        predicate=lambda e: e.get("consult") is True
        and e.get("name") == "connected",
        timeout=10)


async def _wait_webhook(webhook_server, event_type: str, predicate=None,
                        timeout: float = 20.0):
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        for e in webhook_server.receiver.all_events():
            if e.event_type == event_type and (
                    predicate is None or predicate(e)):
                return e
        await asyncio.sleep(0.3)
    return None


async def _active_calls(api) -> list[dict]:
    resp = await api.get("/api/cc/calls/active")
    if isinstance(resp, list):
        return resp
    if isinstance(resp, dict):
        return resp.get("calls") or []
    return []


async def _wait_active_calls(api, count: int, timeout: float = 12.0) -> list[dict]:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        calls = await _active_calls(api)
        if len(calls) == count:
            return calls
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"expected {count} active calls, got {len(calls)}: {await _active_calls(api)}")


async def _end_all(api) -> None:
    for c in await _active_calls(api):
        try:
            await api.post(f"/api/cc/calls/{c.get('call_id')}/end", {})
        except Exception:  # noqa: BLE001
            pass


async def _agent_status(api, agent_id: str) -> str | None:
    try:
        resp = await api.get(f"/api/cc/agents/{agent_id}")
    except Exception:  # noqa: BLE001
        return None
    if isinstance(resp, dict):
        return resp.get("status") or (resp.get("agent") or {}).get("status")
    return None


async def _wait_agent_status(api, agent_id: str, want: str,
                             timeout: float = 20.0) -> bool:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if (await _agent_status(api, agent_id)) == want:
            return True
        await asyncio.sleep(0.5)
    return False


async def _conference_rooms(api) -> list[dict]:
    resp = await api.get("/api/cc/transfer-targets")
    return resp.get("conferences") or []


# ---------------------------------------------------------------------------
# spectral glue
# ---------------------------------------------------------------------------

def _tone_gain(rec: Path, freq: float, lo: float, hi: float) -> float:
    samples, sr = read_wav_mono(rec)
    return band_gain_db(samples, sr, freq, t0=lo, t1=hi)


def _align(rec: Path, freq: float = F_AGENT) -> float:
    return rec_align_offset(rec, freq)


def _present(rec, freq, lo, hi, th=-12.0) -> bool:
    return _tone_gain(rec, freq, lo, hi) >= th


def _absent(rec, freq, lo, hi, th=-20.0) -> bool:
    return _tone_gain(rec, freq, lo, hi) <= th


async def _flush_rec(rec: Path, timeout: float = 45.0) -> Path:
    got = await wait_recording_async(rec, timeout=timeout)
    assert got is not None, f"recording never flushed: {rec}"
    return got


# ---------------------------------------------------------------------------
# G1: blind REFER → CC-busy agent → retry → success
# ---------------------------------------------------------------------------

async def test_g1_blind_refer_busy_agent_then_retry(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    # 1003 goes busy: an established call with 1004 makes the phone reject
    # the transfer INVITE with 486 ("already busy").
    busy = _restsend(avail_pbx, TARGET, 25103)
    peer = _restsend(avail_pbx, BUSY_PEER, 25104)
    await busy.start()
    await peer.start()
    try:
        assert await busy.register(expires=120)
        assert await peer.register(expires=120)
        await _pair_up(busy, peer, avail_pbx)

        ok_target = _echo_bot(sipbot_pool, avail_pbx, h.ua_port(15114), ECHO)
        await h.wait_registered(ok_target, ECHO)

        agent, cust, _t = await _establish(avail_pbx, sipbot_pool, tmp_path)
        baseline = agent.stats_index()
        try:
            refer = await agent.refer_blind(f"sip:{TARGET}@{avail_pbx.sip_addr}")
            assert refer and refer.get("status") == 486, (
                f"busy agent must fail the REFER with 486, got {refer}")
            await asyncio.sleep(2.0)
            state = await agent.current_state()
            assert state in ("connected", "hold"), (
                f"original call must survive: {state}")
            await agent.wait_media_flow(since=baseline, min_received=10,
                                        timeout=10, label="1002-survived")

            # Retry on the idle target → success.
            refer2 = await agent.refer_blind(f"sip:{ECHO}@{avail_pbx.sip_addr}")
            assert refer2 and refer2.get("status") in (200, 202), (
                f"retry REFER rejected: {refer2}")
            await agent.wait_state("ended", timeout=15)
            await _wait_webhook(
                webhook_server, "call_transferred",
                predicate=lambda e: ECHO in ((e.payload or {}).get("transfer_target") or ""),
                timeout=15) or pytest.fail("call_transferred missing")
            # Release the busy pair first (it is a separate live session).
            await busy.hangup()
            await asyncio.sleep(1.0)
            await _wait_active_calls(api, 1)
            calls = await _active_calls(api)
            await api.post(f"/api/cc/calls/{calls[0].get('call_id')}/end", {})
            await _wait_active_calls(api, 0, timeout=20)
        finally:
            await agent.stop()
    finally:
        await busy.stop()
        await peer.stop()


# ---------------------------------------------------------------------------
# G2a: queue + agent on BREAK → recovers → dispatched
# ---------------------------------------------------------------------------

async def test_g2a_queue_break_agent_recovers(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    phone = _restsend(avail_pbx, BREAK_AGENT, 25116)
    await phone.start()
    try:
        assert await phone.register(expires=120)
        await phone.publish_idle()
        assert await api.post(f"/api/cc/agents/{BREAK_AGENT}/status",
                              {"status": "away:break"}), "break rejected"
        await asyncio.sleep(1.0)

        cust = _customer(sipbot_pool, avail_pbx, tmp_path,
                         target=f"sip:{AV_NUM}@{avail_pbx.sip_addr}",
                         port_base=20402)
        # Queued (agent on break → not schedulable): no dispatch within 8s.
        assert await phone.wait_gone("sip_incoming", quiet_secs=8), (
            "agent on break must NOT be dispatched")
        ev = await _wait_webhook(
            webhook_server, "skill_group_call_queued",
            predicate=lambda e: (e.payload or {}).get("skill_group_id") == SKILL_AV,
            timeout=15)
        assert ev, f"customer never queued: {webhook_server.receiver.event_types()}"

        # Agent recovers → dispatched → answers.
        assert await api.post(f"/api/cc/agents/{BREAK_AGENT}/status",
                              {"status": "idle"}), "idle rejected"
        assert await phone.answer(timeout=25), (
            f"recovered agent never dispatched:\n{phone.stderr_text()[-500:]}")
        await phone.wait_media_flow(min_received=10, timeout=10, label="1016")

        assigned = await _wait_webhook(
            webhook_server, "skill_group_agent_assigned",
            predicate=lambda e: (e.payload or {}).get("agent_id") == BREAK_AGENT,
            timeout=15)
        assert assigned, "agent_assigned missing after recovery"

        await phone.hangup()
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=30)
    finally:
        await phone.stop()


# ---------------------------------------------------------------------------
# G2b: queue + agent stays on BREAK → wait timeout → fallback 486
# ---------------------------------------------------------------------------

async def test_g2b_queue_break_agent_fallback_release(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    phone = _restsend(avail_pbx, BREAK_AGENT, 25116)
    await phone.start()
    try:
        assert await phone.register(expires=120)
        await phone.publish_idle()
        await api.post(f"/api/cc/agents/{BREAK_AGENT}/status",
                       {"status": "away:break"})
        await asyncio.sleep(1.0)

        cust = _customer(sipbot_pool, avail_pbx, tmp_path,
                         target=f"sip:{AV_NUM}@{avail_pbx.sip_addr}",
                         port_base=20404, hangup=90)
        # Wait until the customer is ACTUALLY queued (a first-instant poll
        # would mistake "not queued yet" for "released").
        ev = await _wait_webhook(
            webhook_server, "skill_group_call_queued",
            predicate=lambda e: (e.payload or {}).get("skill_group_id") == SKILL_AV,
            timeout=15)
        assert ev, f"customer never queued: {webhook_server.receiver.event_types()}"
        # wait_timeout(20s) + fallback 486 → the queued customer is released.
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 45
        released = False
        while loop.time() < deadline:
            calls = await _active_calls(api)
            if not any(c.get("caller") and CUSTOMER in c["caller"]
                       for c in calls):
                released = True
                break
            await asyncio.sleep(0.5)
        assert released, (
            f"queued customer must be released by the wait-timeout fallback: "
            f"{await _active_calls(api)}")
        assert webhook_server.receiver.count("call_hangup") >= 1, (
            "call_hangup missing after the fallback release")
    finally:
        await phone.stop()


# ---------------------------------------------------------------------------
# G3: queue ring-timeout rotation → final agent answers
# ---------------------------------------------------------------------------

async def test_g3_queue_rotation_after_ring_timeouts(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    ra = _ringing_restsend(avail_pbx, ROT_A, 25117)
    rb = _ringing_restsend(avail_pbx, ROT_B, 25118)
    await ra.start()
    await rb.start()
    ok = _echo_bot(sipbot_pool, avail_pbx, h.ua_port(15119), ROT_OK)
    await h.wait_registered(ok, ROT_OK)
    try:
        assert await ra.register(expires=120), "1017 register failed"
        assert await rb.register(expires=120), "1018 register failed"
        await ra.publish_idle()
        await rb.publish_idle()
        await asyncio.sleep(1.0)

        cust = _customer(sipbot_pool, avail_pbx, tmp_path,
                         target=f"sip:{ROT_NUM}@{avail_pbx.sip_addr}",
                         port_base=20406, hangup=120)
        # 1017 rings 6s → 1018 rings 6s → 1019 answers (~2s) — the rotation
        # must land on the answering agent within ~25s.
        connected = await _wait_webhook(
            webhook_server, "queue_agent_connected", timeout=30)
        assert connected, "rotation never connected an agent"
        agent_id = (connected.payload or {}).get("agent_id")
        assert agent_id == ROT_OK, (
            f"rotation must land on {ROT_OK}, got {agent_id}")
        await _wait_bot_frames(ok, min_frames=40, timeout=20, label=ROT_OK)

        # The failed attempts must not strand Ringing state.
        for aid in (ROT_A, ROT_B):
            assert await _wait_agent_status(api, aid, "idle", timeout=15), (
                f"agent {aid} stuck after ring timeout")

        await _drain(api, timeout=30)
    finally:
        await ra.stop()
        await rb.stop()


async def _wait_bot_frames(bot, min_frames: int = 40, timeout: float = 20.0,
                           label: str = "") -> dict:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    last = None
    while loop.time() < deadline:
        aq = bot.get_audio_quality()
        if aq and aq.get("total_frames", 0) >= min_frames:
            return aq
        last = aq
        await asyncio.sleep(0.3)
    raise AssertionError(
        f"{label or 'bot'}: no audio frames: {last}\n{bot.output[-800:]}")


# ---------------------------------------------------------------------------
# G4: consult → busy target fails → recover → re-consult → retrieve
# ---------------------------------------------------------------------------

async def test_g4_consult_busy_target_then_reconsult(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    busy = _restsend(avail_pbx, TARGET, 25103)
    peer = _restsend(avail_pbx, BUSY_PEER, 25104)
    await busy.start()
    await peer.start()
    try:
        assert await busy.register(expires=120)
        assert await peer.register(expires=120)
        await _pair_up(busy, peer, avail_pbx)

        ok_target = _echo_bot(sipbot_pool, avail_pbx, h.ua_port(15114), ECHO)
        await h.wait_registered(ok_target, ECHO)

        agent, cust, _t = await _establish(avail_pbx, sipbot_pool, tmp_path)
        baseline = agent.stats_index()
        try:
            # Consult the BUSY target: the leg is rejected 486 → failed.
            await agent.cmd({"cmd": "sip_call",
                             "remote_uri": f"sip:{TARGET}@{avail_pbx.sip_addr}",
                             "consult": True})
            await asyncio.sleep(3.0)
            errors = [e for e in agent.events if e.get("evt") == "error"]
            # The engine either reports the failed consult or auto-resumed.
            if (await agent.current_state()) != "connected":
                await agent.resume_call()
            assert await agent.wait_recovered(timeout=12) or \
                (await agent.current_state()) == "connected", (
                f"main call must recover after the failed consult: "
                f"{agent.current_state()}")
            await agent.wait_media_flow(since=baseline, min_received=10,
                                        timeout=10, label="1002-recovered")

            # Re-consult the idle target → retrieve → media verified by DTMF.
            consult = await agent.consult(f"sip:{ECHO}@{avail_pbx.sip_addr}")
            assert consult, "re-consult never connected"
            await agent.hangup_consult()
            assert await agent.ensure_recovered(timeout=20)
            assert await _dtmf_verified(agent, cust, "6"), (
                "customer must receive DTMF after retrieve")

            await agent.hangup()
            await busy.hangup()  # release the busy-pair session
            await peer.hangup()
            await _drain(api, timeout=25)
        finally:
            await agent.stop()
    finally:
        await busy.stop()
        await peer.stop()


async def _dtmf_verified(agent: RestsendAgent, bot, digit: str,
                         attempts: int = 3) -> bool:
    """Send DTMF (retrying — the media path may still be settling right
    after a hold/resume cycle) until the far bot logs the digit."""
    for i in range(attempts):
        await agent.send_dtmf(digit)
        if await _wait_dtmf(bot, digit, timeout=5):
            return True
        await asyncio.sleep(1.0)
    return False


async def _wait_dtmf(bot, digit: str, timeout: float = 10.0) -> bool:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if digit in bot.get_dtmf_digits():
            return True
        await asyncio.sleep(0.2)
    return False


# ---------------------------------------------------------------------------
# H5: consult/retrieve cycle ×2 → attended transfer (spectral per cycle)
# ---------------------------------------------------------------------------

async def test_h5_consult_retrieve_cycles_then_transfer(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    consult_partner = _restsend(avail_pbx, TARGET, 25103)
    await consult_partner.start()
    tone_target = None
    try:
        assert await consult_partner.register(expires=120)

        tone730 = tmp_path / "t730.wav"
        generate_sine_wav(tone730, F_TARGET, 90.0, 8000, 0.4)
        rec730 = tmp_path / "t730_rx.wav"
        tone_target = _tone_target(sipbot_pool, avail_pbx, h.ua_port(15115),
                                   TONE730, tone730, record_file=str(rec730))
        await h.wait_registered(tone_target, TONE730)

        agent, cust, t_est = await _establish(avail_pbx, sipbot_pool, tmp_path)
        phases: list[tuple[str, float, float]] = []
        try:
            # ── cycle 1 ──
            c1 = time.monotonic()
            await _consult_with_answer(agent, consult_partner,
                                       f"sip:{TARGET}@{avail_pbx.sip_addr}")
            await asyncio.sleep(2.0)  # clean MOH window for the spectral assert
            r1 = time.monotonic()
            await agent.hangup_consult()
            assert await agent.ensure_recovered(timeout=20)
            e1 = time.monotonic()
            phases.append(("consult1", c1 - t_est, r1 - t_est))
            phases.append(("recover1", r1 - t_est, e1 - t_est))

            # ── cycle 2 ──
            c2 = time.monotonic()
            await _consult_with_answer(agent, consult_partner,
                                       f"sip:{TARGET}@{avail_pbx.sip_addr}")
            await asyncio.sleep(2.0)  # clean MOH window for the spectral assert
            r2 = time.monotonic()
            await agent.hangup_consult()
            assert await agent.ensure_recovered(timeout=20)
            e2 = time.monotonic()
            phases.append(("consult2", c2 - t_est, r2 - t_est))
            phases.append(("recover2", r2 - t_est, e2 - t_est))

            # DTMF still crosses after two hold/resume cycles.
            assert await _dtmf_verified(agent, cust, "3"), "media lost after cycles"

            # ── attended transfer to the 730 Hz target ──
            consult = await agent.consult(f"sip:{TONE730}@{avail_pbx.sip_addr}")
            assert consult, "730 consult never connected"
            await asyncio.sleep(1.0)  # let the consult leg settle
            refer = await agent.refer_attended(
                f"sip:{TONE730}@{avail_pbx.sip_addr}", timeout=20)
            assert refer and refer.get("status") in (200, 202), (
                f"attended REFER rejected: {refer}")
            # The NOTIFY 200 bridges customer↔1015 — the room-path media
            # (re-INVITE + ICE/DTLS) takes ~2-3 s to actually carry audio,
            # so dwell 6 s and sample the LATE part of the window.
            t_done = time.monotonic()
            phases.append(("transferred", t_done - t_est + 1.5,
                           t_done - t_est + 5.5))
            await asyncio.sleep(6.0)

            await agent.hangup()
            await _end_all(api)
            await _wait_active_calls(api, 0, timeout=30)
        finally:
            await agent.stop()

        # ── spectral timeline on the customer recording ──
        rec = await _flush_rec(cust_rec(tmp_path, CUSTOMER))
        off = _align(rec, F_AGENT)
        for name, lo, hi in phases:
            a, b = off + lo + 0.9, off + hi - 0.5
            if b - a < 1.0:
                b = a + 1.0
            if name.startswith("consult"):
                assert _absent(rec, F_AGENT, a, b), (
                    f"{name}: agent tone must be absent (customer on MOH), "
                    f"gain={_tone_gain(rec, F_AGENT, a, b):.1f}")
            elif name.startswith("recover"):
                assert _present(rec, F_AGENT, a, b), (
                    f"{name}: agent tone must return after retrieve, "
                    f"gain={_tone_gain(rec, F_AGENT, a, b):.1f}")
            elif name == "transferred":
                assert _present(rec, F_TARGET, a, b), (
                    "after the transfer the customer must hear the 730 Hz "
                    f"target, gain={_tone_gain(rec, F_TARGET, a, b):.1f}")
                assert _absent(rec, F_AGENT, a, b), (
                    "after the transfer the agent must be gone")
        assert not await _conference_rooms(api), "no room may survive"
    finally:
        await consult_partner.stop()
        if tone_target:
            tone_target.terminate()


def cust_rec(tmp_path: Path, username: str) -> Path:
    return tmp_path / f"cust_{username}_rx.wav"


# ---------------------------------------------------------------------------
# I — one agent serves MANY consecutive calls (dispatch accuracy +
# schedulability across blind REFER / consult transfer cycles)
# ---------------------------------------------------------------------------

async def _dispatch_one(pbx, sipbot_pool, tmp_path, webhook_server, phone,
                        api, idx: int, timeout: float = 30.0,
                        port_base: int = 20500):
    """Customer → av queue → dispatched to the phone; returns the cust bot.
    Asserts the dispatch contract (assigned + agent_id ringing)."""
    webhook_server.receiver.clear()
    # Prior cases may have left the agent on break/wrapup — force Idle and
    # confirm before dialing (a non-Idle agent is unschedulable).
    try:
        await api.post(f"/api/cc/agents/{BREAK_AGENT}/status",
                       {"status": "idle"})
    except Exception:  # noqa: BLE001
        pass
    assert await _wait_agent_status(api, BREAK_AGENT, "idle", timeout=20), (
        f"agent {BREAK_AGENT} never reached Idle before call {idx}")
    cust = _customer(sipbot_pool, pbx, tmp_path,
                     username=f"10{50 + idx}",
                     target=f"sip:{AV_NUM}@{pbx.sip_addr}",
                     port_base=port_base)
    assert await phone.answer(timeout=timeout), (
        f"call {idx} never dispatched:\n{phone.stderr_text()[-500:]}")
    assigned = await _wait_webhook(
        webhook_server, "skill_group_agent_assigned",
        predicate=lambda e: (e.payload or {}).get("agent_id") == BREAK_AGENT,
        timeout=10)
    assert assigned, f"call {idx}: agent_assigned(1016) missing"
    return cust


async def test_i1_agent_serves_three_calls_state_chain(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """Three consecutive queued calls to ONE agent: every cycle must walk
    idle → (assigned/ringing/busy) → wrapup → idle, and the agent must stay
    schedulable throughout (no stuck Busy, no dead dispatch)."""
    _skip_without_cli()
    phone = _restsend(avail_pbx, BREAK_AGENT, 25116)
    await phone.start()
    try:
        assert await phone.register(expires=120)
        await phone.publish_idle()
        await asyncio.sleep(1.0)

        for i in (1, 2, 3):
            cust = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                       webhook_server, phone,
                                        api, i)
            assert await _wait_agent_status(api, BREAK_AGENT, "busy",
                                            timeout=10), (
                f"call {i}: agent must be Busy while connected")
            await phone.wait_media_flow(min_received=10, timeout=10,
                                        label=f"1016-call{i}")
            await phone.hangup()
            assert await _wait_agent_status(api, BREAK_AGENT, "idle",
                                            timeout=20), (
                f"call {i}: agent must return to Idle (wrapup 2s)")
            await _wait_active_calls(api, 0, timeout=20)

        assert not await _conference_rooms(api)
    finally:
        await phone.stop()


async def test_i2_serve_then_blind_refer_then_serve_again(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """Call 1: serve, blind-REFER the customer to 1015, agent released.
    Call 2: the SAME agent must be re-dispatched and serve media again."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    phone = _restsend(avail_pbx, BREAK_AGENT, 25116)
    await phone.start()
    tone730 = tmp_path / "t730.wav"
    generate_sine_wav(tone730, 730.0, 90.0, 8000, 0.4)
    rec730 = tmp_path / "t730_rx.wav"
    tone_target = _tone_target(sipbot_pool, avail_pbx, h.ua_port(15115),
                               TONE730, tone730, record_file=str(rec730))
    try:
        assert await phone.register(expires=120)
        await phone.publish_idle()
        await asyncio.sleep(1.0)

        # ── call 1: serve → blind REFER away ──
        cust1 = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                    webhook_server, phone,
                                        api, 1)
        assert await _wait_agent_status(api, BREAK_AGENT, "busy", timeout=10)
        refer = await phone.refer_blind(f"sip:{TONE730}@{avail_pbx.sip_addr}")
        assert refer and refer.get("status") in (200, 202), (
            f"REFER rejected: {refer}")
        await _wait_webhook(
            webhook_server, "call_transferred",
            predicate=lambda e: TONE730 in ((e.payload or {}).get("transfer_target") or ""),
            timeout=15) or pytest.fail("call_transferred missing")

        # The agent returns to Idle; the customer lives on with 1015.
        assert await _wait_agent_status(api, BREAK_AGENT, "idle", timeout=20), (
            "agent must return to Idle after the blind REFER")
        await asyncio.sleep(2.5)  # transfer media settles

        await _end_all(api)
        rec = await _flush_rec(tmp_path / "cust_1051_rx.wav")
        samples, sr = read_wav_mono(rec)
        wa, wb = samples.size / sr - 4.5, samples.size / sr - 1.0
        assert _tone_gain(rec, F_TARGET, wa, wb) >= -14, (
            "customer must hear the 730 Hz target after the blind REFER")

        # ── call 2: the agent is schedulable again ──
        webhook_server.receiver.clear()
        cust2 = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                    webhook_server, phone,
                                        api, 2)
        assert await _wait_agent_status(api, BREAK_AGENT, "busy", timeout=10)
        await phone.wait_media_flow(min_received=10, timeout=10,
                                    label="1016-call2")
        await phone.hangup()
        assert await _wait_agent_status(api, BREAK_AGENT, "idle", timeout=20)
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=30)
        assert not await _conference_rooms(api)
    finally:
        await phone.stop()
        tone_target.terminate()


async def test_i3_serve_then_consult_transfer_then_serve_again(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """Call 1: serve, consult-transfer (BC) the customer to 1015, agent
    released. Call 2: the SAME agent re-dispatched and serving again."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    phone = _restsend(avail_pbx, BREAK_AGENT, 25116)
    await phone.start()
    tone730 = tmp_path / "t730.wav"
    generate_sine_wav(tone730, 730.0, 90.0, 8000, 0.4)
    rec730 = tmp_path / "t730_rx.wav"
    tone_target = _tone_target(sipbot_pool, avail_pbx, h.ua_port(15115),
                               TONE730, tone730, record_file=str(rec730))
    try:
        assert await phone.register(expires=120)
        await phone.publish_idle()
        await asyncio.sleep(1.0)

        # ── call 1: serve → consult → attended transfer ──
        cust1 = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                    webhook_server, phone, api, 1)
        assert await _wait_agent_status(api, BREAK_AGENT, "busy", timeout=10)
        consult = await phone.consult(f"sip:{TONE730}@{avail_pbx.sip_addr}")
        assert consult, "consult never connected"
        refer = await phone.refer_attended(
            f"sip:{TONE730}@{avail_pbx.sip_addr}", timeout=20)
        assert refer and refer.get("status") in (200, 202), (
            f"attended REFER rejected: {refer}")
        await _wait_webhook(
            webhook_server, "call_transferred",
            predicate=lambda e: TONE730 in ((e.payload or {}).get("transfer_target") or ""),
            timeout=20) or pytest.fail("call_transferred missing")

        assert await _wait_agent_status(api, BREAK_AGENT, "idle", timeout=20), (
            "agent must return to Idle after the consult transfer")
        await asyncio.sleep(2.5)  # transfer media settles

        await _end_all(api)
        rec = await _flush_rec(tmp_path / "cust_1051_rx.wav")
        samples, sr = read_wav_mono(rec)
        wa, wb = samples.size / sr - 4.5, samples.size / sr - 1.0
        assert _tone_gain(rec, F_TARGET, wa, wb) >= -14, (
            "customer must hear the 730 Hz target after the consult transfer")

        # ── call 2: schedulable again ──
        webhook_server.receiver.clear()
        cust2 = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                    webhook_server, phone, api, 2)
        assert await _wait_agent_status(api, BREAK_AGENT, "busy", timeout=10)
        await phone.wait_media_flow(min_received=10, timeout=10,
                                    label="1016-call2")
        await phone.hangup()
        assert await _wait_agent_status(api, BREAK_AGENT, "idle", timeout=20)
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=30)
        assert not await _conference_rooms(api)
    finally:
        await phone.stop()
        tone_target.terminate()


async def test_i4_serve_transfer_stress_then_dispatch(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """Three served calls, each ended by a transfer (blind / consult /
    blind alternating), then a FOURTH plain call must still dispatch —
    the state chain survives rapid transfer cycling."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    phone = _restsend(avail_pbx, BREAK_AGENT, 25116)
    await phone.start()
    tone730 = tmp_path / "t730.wav"
    generate_sine_wav(tone730, 730.0, 120.0, 8000, 0.4)
    rec730 = tmp_path / "t730_rx.wav"
    tone_target = _tone_target(sipbot_pool, avail_pbx, h.ua_port(15115),
                               TONE730, tone730, record_file=str(rec730))
    echo = _echo_bot(sipbot_pool, avail_pbx, h.ua_port(15114), ECHO)
    await h.wait_registered(echo, ECHO)
    try:
        assert await phone.register(expires=120)
        await phone.publish_idle()
        await asyncio.sleep(1.0)

        for rnd, mode in enumerate(("blind", "consult", "blind"), 1):
            cust = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                       webhook_server, phone,
                                        api, rnd,
                                       port_base=20520 + rnd * 2)
            assert await _wait_agent_status(api, BREAK_AGENT, "busy",
                                            timeout=10)
            target = f"sip:{TONE730}@{avail_pbx.sip_addr}"
            if mode == "blind":
                refer = await phone.refer_blind(target)
            else:
                consult = await phone.consult(target)
                assert consult, f"round {rnd}: consult failed"
                refer = await phone.refer_attended(target, timeout=20)
            assert refer and refer.get("status") in (200, 202), (
                f"round {rnd}: REFER failed: {refer}")
            assert await _wait_agent_status(api, BREAK_AGENT, "idle",
                                            timeout=25), (
                f"round {rnd}: agent must return to Idle after the transfer")
            await _end_all(api)
            await _wait_active_calls(api, 0, timeout=30)

        # Round 4: plain service — dispatch must still work.
        webhook_server.receiver.clear()
        cust4 = await _dispatch_one(avail_pbx, sipbot_pool, tmp_path,
                                    webhook_server, phone,
                                        api, 4,
                                    port_base=20530)
        assert await _wait_agent_status(api, BREAK_AGENT, "busy", timeout=10)
        await phone.wait_media_flow(min_received=10, timeout=10,
                                    label="1016-call4")
        await phone.hangup()
        assert await _wait_agent_status(api, BREAK_AGENT, "idle", timeout=20)
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=30)
        assert not await _conference_rooms(api)
    finally:
        await phone.stop()
        tone_target.terminate()


# ---------------------------------------------------------------------------
# H6: consult target BYE mid-consult → auto-resume → re-consult
# ---------------------------------------------------------------------------

async def test_h6_consult_target_bye_then_reconsult(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    consult_partner = _restsend(avail_pbx, TARGET, 25103)
    await consult_partner.start()
    tone_target = None
    try:
        assert await consult_partner.register(expires=120)
        tone730 = tmp_path / "t730.wav"
        generate_sine_wav(tone730, F_TARGET, 90.0, 8000, 0.4)
        rec730 = tmp_path / "t730_rx.wav"
        tone_target = _tone_target(sipbot_pool, avail_pbx, h.ua_port(15115),
                                   TONE730, tone730, record_file=str(rec730))
        await h.wait_registered(tone_target, TONE730)

        agent, cust, t_est = await _establish(avail_pbx, sipbot_pool, tmp_path)
        phases = []
        try:
            c1 = time.monotonic()
            await _consult_with_answer(agent, consult_partner,
                                       f"sip:{TARGET}@{avail_pbx.sip_addr}")
            await asyncio.sleep(2.0)  # clean MOH window for the spectral assert
            b1 = time.monotonic()
            # The consult party hangs up mid-consult.
            await consult_partner.hangup()
            assert await agent.wait_recovered(timeout=15), (
                f"main must auto-resume when the consult party leaves: "
                f"{agent.current_state()}")
            e1 = time.monotonic()
            phases.append(("consult1", c1 - t_est, b1 - t_est))
            phases.append(("recover1", b1 - t_est, e1 - t_est))
            assert await _dtmf_verified(agent, cust, "4"), "media lost"

            # Re-consult the tone target → retrieve.
            c2 = time.monotonic()
            assert await agent.consult(f"sip:{TONE730}@{avail_pbx.sip_addr}")
            await asyncio.sleep(2.0)  # clean MOH window for the spectral assert
            r2 = time.monotonic()
            await agent.hangup_consult()
            assert await agent.ensure_recovered(timeout=20)
            e2 = time.monotonic()
            phases.append(("consult2", c2 - t_est, r2 - t_est))
            phases.append(("recover2", r2 - t_est, e2 - t_est))
            assert await _dtmf_verified(agent, cust, "5")

            await agent.hangup()
            await _end_all(api)
            await _wait_active_calls(api, 0, timeout=20)
        finally:
            await agent.stop()

        rec = await _flush_rec(cust_rec(tmp_path, CUSTOMER))
        off = _align(rec, F_AGENT)
        for name, lo, hi in phases:
            a, b = off + lo + 0.9, off + hi - 0.5
            if b - a < 1.0:
                b = a + 1.0
            if name.startswith("consult"):
                assert _absent(rec, F_AGENT, a, b), (
                    f"{name}: customer must be on MOH during consult")
            else:
                assert _present(rec, F_AGENT, a, b), (
                    f"{name}: agent tone must return after recovery")
    finally:
        await consult_partner.stop()
        if tone_target:
            tone_target.terminate()


# ---------------------------------------------------------------------------
# H7: switch away → held consult leg BYE → customer keeps talking
# ---------------------------------------------------------------------------

async def test_h7_switch_held_leg_bye_customer_continues(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    consult_partner = _restsend(avail_pbx, TARGET, 25103)
    await consult_partner.start()
    try:
        assert await consult_partner.register(expires=120)

        agent, cust, t_est = await _establish(avail_pbx, sipbot_pool, tmp_path)
        phases = []
        try:
            await asyncio.sleep(2.0)  # clean pre-switch 920 Hz window
            c1 = time.monotonic()
            await _consult_with_answer(agent, consult_partner,
                                       f"sip:{TARGET}@{avail_pbx.sip_addr}")
            s1 = time.monotonic()
            await agent.switch_consult()
            party = await agent.wait_consult_party(customer_active=True,
                                                   timeout=15)
            assert party, "switch back to customer failed"
            sw = time.monotonic()
            phases.append(("pre_switch", 0.8, s1 - t_est - 0.4))
            # The held consult party hangs up.
            await consult_partner.hangup()
            h1 = time.monotonic()
            e1 = time.monotonic()
            await asyncio.sleep(2.0)
            phases.append(("post_bye", h1 - t_est, e1 - t_est + 1.5))

            # The customer keeps hearing the agent the whole time.
            assert await _dtmf_verified(agent, cust, "7"), (
                "customer media must survive the held-leg BYE")

            await agent.hangup()
            await _end_all(api)
            await _wait_active_calls(api, 0, timeout=20)
        finally:
            await agent.stop()

        rec = await _flush_rec(cust_rec(tmp_path, CUSTOMER))
        off = _align(rec, F_AGENT)
        for name, lo, hi in phases:
            a, b = off + lo + 0.8, off + hi - 0.5
            if b - a < 1.0:
                b = a + 1.0
            assert _present(rec, F_AGENT, a, b), (
                f"{name}: customer must keep hearing the agent "
                f"(gain={_tone_gain(rec, F_AGENT, a, b):.1f})")
    finally:
        await consult_partner.stop()


# ---------------------------------------------------------------------------
# H8: 3-way conference mixing matrix → one party leaves
# ---------------------------------------------------------------------------

async def test_h8_conference_mixing_and_party_leave(
        avail_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    # The 3rd party is a restsend phone (730 Hz tone): it can hang up
    # GRACEFULLY (BYE) when it leaves, and its RX packet counter proves it
    # hears the mix. (A sipbot callee cannot BYE on demand.)
    third = _restsend(avail_pbx, TONE730, 25115, tone_hz=730)
    await third.start()

    agent, cust, _t = await _establish(avail_pbx, sipbot_pool, tmp_path)
    merge_mid = None
    leave_off = None
    try:
        assert await third.register(expires=120)
        consult = await _consult_with_answer(
            agent, third, f"sip:{TONE730}@{avail_pbx.sip_addr}")
        assert consult, "consult leg never connected"
        await agent.conference(
            f"sip:conf-factory@{avail_pbx.host}:{avail_pbx.sip_port}")
        await _wait_webhook(webhook_server, "conference_joined", timeout=35) or \
            pytest.fail("conference_joined missing")
        merge_mid = time.monotonic()
        await third.wait_media_flow(min_received=30, timeout=15,
                                    label="1015-3way")
        await asyncio.sleep(2.0)  # sustain the 3-way for clean windows

        leave = time.monotonic()
        await third.hangup()  # graceful BYE out of the room
        await asyncio.sleep(3.0)  # 2-party tail
        leave_off = leave - _t

        await agent.hangup()
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=30)
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 15
        while loop.time() < deadline:
            if not await _conference_rooms(api):
                break
            await asyncio.sleep(0.4)
        else:
            pytest.fail(f"room survived: {await _conference_rooms(api)}")
    finally:
        await agent.stop()
        await third.stop()

    # ── mixing matrix on the customer recording ──
    m0 = merge_mid - _t + 0.8
    m1 = m0 + 2.4
    crec = await _flush_rec(cust_rec(tmp_path, CUSTOMER))
    off = _align(crec, F_AGENT)
    ca, cb = off + m0, off + m1
    # A 3-way mix splits each source's energy — a presence threshold of
    # -32 dB still sits far above the MOH-only floor (≈ -55).
    assert _present(crec, F_AGENT, ca, cb, th=-32), (
        f"3-way: customer must hear the agent (gain="
        f"{_tone_gain(crec, F_AGENT, ca, cb):.1f})")
    assert _present(crec, F_TARGET, ca, cb, th=-32), (
        f"3-way: customer must hear the 730 Hz target (gain="
        f"{_tone_gain(crec, F_TARGET, ca, cb):.1f})")

    # After the target leaves: its tone is gone from the customer, the
    # agent's tone remains.
    la, lb = off + leave_off + 0.6, off + leave_off + 2.4
    assert _absent(crec, F_TARGET, la, lb, th=-18), (
        f"target tone must stop after it leaves (gain="
        f"{_tone_gain(crec, F_TARGET, la, lb):.1f})")
    assert _present(crec, F_AGENT, la, lb), (
        "agent tone must remain after the target leaves")
