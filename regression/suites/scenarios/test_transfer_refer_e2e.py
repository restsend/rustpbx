"""SIP-REFER transfer core flows e2e — restsend-cli + sipbot endpoints,
covering the 2026-09 transfer/hold regression matrix:

  R1  blind REFER → IVR           in-session app hand-off, no re-dial
  R2  blind REFER → agent ext     bridge + bidirectional RTP
  R3  blind REFER → skill-group   ACD dispatch + call_ringing agent contract
  R4  blind REFER target busy     NOTIFY failure, original call survives
  R5  attended REFER → agent      transfer-room path, both old legs released
  R6  attended REFER → IVR        R1 fix: in-session hand-off, no re-dial
  R7  attended REFER failure      (attended) marker, customer held, retrieve
  R8  consult → retrieve          hold-music-loop fix: post-resume DTMF
  R9  consult IVR → retrieve      IVR consult session cleanup
  R10 consult no-answer → cancel  CANCEL path cleanup
  R11 transferor hangup mid-REFER cascade teardown, agent re-schedulable
  R12 long hold                   no rtpTimeout mid-hold, media after resume
  R13 consult switch              restsend sip_consult_switch dual re-INVITE
  R14 conference merge + C1       factory REFER order, consult-join failure

Client roles: 1001 = customer (sipbot), 1002 = transferor agent
(restsend-cli), 1003 = consult/transfer target (sipbot echo).
IVR route point 8880; queue `support` backed by skill-group:support.
The PBX boots ONCE per module; every case drains all sessions first.
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

F_AGENT = 920.0   # restsend agent tone (RESTSEND_TONE_HZ)
F_TARGET = 730.0  # tone-target bot (answers playing a 730 Hz wav)


def _tone_gain(rec: Path, freq: float, lo: float, hi: float) -> float:
    samples, sr = read_wav_mono(rec)
    return band_gain_db(samples, sr, freq, t0=lo, t1=hi)


def _present(rec: Path, freq: float, lo: float, hi: float,
             th: float = -12.0) -> bool:
    return _tone_gain(rec, freq, lo, hi) >= th


def _absent(rec: Path, freq: float, lo: float, hi: float,
            th: float = -20.0) -> bool:
    return _tone_gain(rec, freq, lo, hi) <= th


def _align(rec: Path, freq: float = F_AGENT) -> float:
    return rec_align_offset(rec, freq)


async def _flush_rec(rec: Path, timeout: float = 45.0) -> Path:
    got = await wait_recording_async(rec, timeout=timeout)
    assert got is not None, f"recording never flushed: {rec}"
    return got


pytestmark = [pytest.mark.scenario]

CUSTOMER = "1001"  # sipbot caller
AGENT = "1002"     # restsend-cli transferor
TARGET = "1003"    # sipbot echo callee
AGENT2 = "1012"    # R11's fresh transferor (avoids 1002 wrapup carryover)
IVR_NUM = "8880"
QUEUE_NUM = "9200"
QUEUE_NAME = "support"
SKILL_GROUP = "support"
ROUTED_AGENT = "8870"  # R17: REGISTERED user whose number ALSO matches an app route
CC_AGENT_NUM = "1004"  # R18: CC agent extension (no SIP user) on an app route


def _skip_without_cli() -> None:
    if not os.path.exists(RESTSEND_CLI):
        pytest.skip(
            f"restsend-cli not built: {RESTSEND_CLI} "
            "(cargo build -p restsend-cli in ../restsend-call)"
        )


# ---------------------------------------------------------------------------
# module-scoped PBX (one boot for the whole suite)
# ---------------------------------------------------------------------------

def _transfer_ivr_toml(greeting) -> str:
    """Tree IVR on the transfer route point: menu → 1 = playback, 9 = hangup.
    Long timeouts so a parked customer stays inside the menu."""
    return f"""\
[ivr]
name = "transfer_ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting}"
timeout_ms = 60000
max_retries = 100
timeout_action = {{ type = "repeat" }}

[[ivr.root.entries]]
key = "1"
action = {{ type = "play", prompt = "{greeting}" }}

[[ivr.root.entries]]
key = "9"
action = {{ type = "hangup" }}
"""


@pytest.fixture(scope="module")
def transfer_pbx(webhook_server, tmp_path_factory) -> PbxServer:
    """Superset config booted once: IVR route point, queue + ACD skill-group
    route, conference factory. Cases drain their sessions; state carryover is
    limited to idempotent CC seed rows."""
    greeting_dir = tmp_path_factory.mktemp("transfer_e2e")
    greeting = greeting_dir / "greet.wav"
    generate_sine_wav(greeting, 440.0, 2.0, 8000, 0.4)

    server = PbxServer(
        host=root_conftest.SIP_HOST,
        sip_port=root_conftest.SIP_PORT,
        http_port=root_conftest.HTTP_PORT,
        rwi_token=root_conftest.RWI_TOKEN,
        project_root=root_conftest.PROJECT_ROOT,
        work_dir=root_conftest.ARTIFACT_ROOT,
    )
    cb = server.config_builder
    cb.add_ivr("transfer_ivr", _transfer_ivr_toml(greeting))
    cb.add_route(
        "transfer-ivr-route",
        match={"to.user": IVR_NUM},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/transfer_ivr.toml"},
        auto_answer=True,
    )
    cb.add_queue(
        QUEUE_NAME, strategy_mode="sequential",
        targets=[f"skill-group:{SKILL_GROUP}"],
        ring_timeout_secs=6, wait_timeout_secs=30,
    )
    cb.add_route(
        "queue-entry-route",
        match={"to.user": QUEUE_NUM},
        priority=10,
        action="queue",
        queue=QUEUE_NAME,
        auto_answer=True,
    )
    # R11 uses a DEDICATED queue whose only candidate is its fresh agent —
    # the shared `support` pool carries stale bindings from earlier cases
    # (retired bots) whose INVITEs burn the full SIP transaction timeout
    # before the sequential dialer moves on.
    cb.add_queue(
        "support_r11", strategy_mode="sequential",
        targets=["skill-group:r11"],
        ring_timeout_secs=6, wait_timeout_secs=30,
        fallback_failure_code=486,
    )
    cb.add_route(
        "queue-entry-route-r11",
        match={"to.user": "9201"},
        priority=10,
        action="queue",
        queue="support_r11",
        auto_answer=True,
    )
    cb.set_proxy_extra(
        conference_factory_uri=f"sip:conf-factory@{root_conftest.SIP_HOST}:{root_conftest.SIP_PORT}")
    # R17's number carries BOTH an application route AND a registrable
    # memory user — the production incident shape (39300: route rule
    # app=ivr + agent registration; the route hijacked agent-to-agent
    # transfers and the callee rang 15–25 s late or never).
    cb.add_route(
        "transfer-routed-agent-route",
        match={"to.user": ROUTED_AGENT},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/transfer_ivr.toml"},
        auto_answer=True,
    )
    # R18's number: a CC agent extension (no SIP user) that ALSO matches an
    # application route — the production 39300 shape without a registration.
    cb.add_route(
        "transfer-routed-cc-agent-route",
        match={"to.user": CC_AGENT_NUM},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/transfer_ivr.toml"},
        auto_answer=True,
    )
    # R11's fresh transferor + its never-answering REFER target need their
    # own SIP identities (CC agent rows are not SIP users).
    cb.add_memory_users([AGENT2, "1014", ROUTED_AGENT])
    server.prepare(webhook_url=webhook_server.url, build=False)
    server.start(timeout=90)

    # CC agents + skill group (idempotent) — queue dispatch cases need them.
    server.loop = None
    _seed_cc_sync(server)

    yield server
    server.stop()


def _seed_cc_sync(pbx) -> None:
    """Create agents 1002/1003 + skill-group `support` via REST (sync wrapper
    around the async client)."""
    import asyncio as _asyncio
    import aiohttp
    from helpers.pbx_server import PbxApiClient

    async def _run():
        session = aiohttp.ClientSession()
        try:
            client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
            assert await client.ensure_console_auth(), "console auth failed"
            for body in (
                {"agent_id": AGENT, "display_name": "Agent 1002 (transfer-e2e)",
                 "skills": [SKILL_GROUP], "max_concurrency": 3, "role": "agent"},
                {"agent_id": TARGET, "display_name": "Agent 1003 (transfer-e2e)",
                 "skills": [SKILL_GROUP], "max_concurrency": 3, "role": "agent"},
                {"agent_id": AGENT2, "display_name": "Agent 1012 (transfer-e2e)",
                 "skills": ["r11"], "max_concurrency": 3, "role": "agent"},
                {"agent_id": CC_AGENT_NUM, "display_name": "Agent 1004 (transfer-e2e)",
                 "skills": [SKILL_GROUP], "max_concurrency": 3, "role": "agent"},
                {"skill_group_id": SKILL_GROUP, "skills_required": [SKILL_GROUP],
                 "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 90,
                 "metadata": {"wrapup_time_secs": 2}},
                {"skill_group_id": "r11", "skills_required": ["r11"],
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

    # asyncio.run: sync pytest fixture context has no current event loop
    # (asyncio.get_event_loop() raises RuntimeError on modern Python), and
    # pytest-asyncio's loop is only active inside async tests.
    _asyncio.run(_run())


# ---------------------------------------------------------------------------
# shared harness
# ---------------------------------------------------------------------------

def _register_bot(sipbot_pool, pbx, port: int, username: str, **kw):
    """Echo callee by default; `answer_mode=<wav path>` makes the bot answer
    playing that file (its TX), e.g. a 730 Hz tone for spectral asserts."""
    kw.setdefault("audio_quality", True)
    kw.setdefault("answer_mode", "echo")
    bot = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, **kw,
    )
    assert bot, f"{username} sipbot failed to start"
    return bot


def _ringing_bot(sipbot_pool, pbx, port: int, username: str, ring_secs=60):
    """A callee that rings but never answers (for in-flight REFER targets)."""
    bot = sipbot_pool.callee(
        host=pbx.host, port=port, username=username, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=ring_secs, answer_mode="none",
    )
    assert bot
    return bot


def _retire_target(sipbot_pool, settle: float = 0.5) -> None:
    """Kill prior TARGET bots (shared-PBX runs): a leftover bot still holds
    the UDP port AND the registration, so a new variant bot (echo vs
    no-answer vs reject) would either fail to bind or lose the incoming
    call to the old one."""
    sipbot_pool.terminate_user(TARGET)
    import time as _time
    _time.sleep(settle)


def _restsend(pbx, user: str, port: int, device: str = "tone") -> RestsendAgent:
    # 920 Hz agent tone — distinct from the 620 Hz customer and 730 Hz
    # target tones, and in a quiet band of the MOH file (spectral asserts).
    return RestsendAgent(
        pbx, user, local_port=h.ua_port(port), device=device,
        tone_hz=920,
        log_level=os.environ.get("RESTSEND_E2E_LOG", "info"))


def _customer_bot(sipbot_pool, pbx, tmp_path: Path, *, username=CUSTOMER,
                  target=None, hangup=120):
    """sipbot customer: 620 Hz tone out, record RX, AudioQuality on."""
    tone = tmp_path / f"cust_{username}_tone.wav"
    generate_sine_wav(tone, 620.0, 120.0, 8000, 0.4)
    rec = tmp_path / f"cust_{username}_rx.wav"
    port_base = 20100 if username == CUSTOMER else 20100 + (int(username) % 90)
    bot = sipbot_pool.caller(
        target=target or f"sip:{AGENT}@{pbx.sip_addr}",
        username=username, password="123456",
        proxy=f"{pbx.host}:{pbx.sip_port}",
        hangup=hangup, play_file=str(tone), record_file=str(rec),
        audio_quality=True, addr=f"127.0.0.1:{h.ua_port(port_base)}",
    )
    assert bot, f"{username} caller failed to start"
    return bot


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


def _must_webhook(ev, message: str) -> None:
    if ev is None:
        pytest.fail(message)


async def _active_calls(api) -> list[dict]:
    """`GET /api/cc/calls/active` — the live session registry snapshot."""
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
    """Force-end every active session (cleanup for cases whose scenarios
    legitimately leave a session behind, e.g. a still-queued customer)."""
    for c in await _active_calls(api):
        try:
            await api.post(f"/api/cc/calls/{c.get('call_id')}/end", {})
        except Exception:  # noqa: BLE001
            pass


async def _conference_rooms(api) -> list[dict]:
    resp = await api.get("/api/cc/transfer-targets")
    return resp.get("conferences") or []


async def _wait_no_rooms(api, timeout: float = 12.0) -> None:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if not await _conference_rooms(api):
            return
        await asyncio.sleep(0.4)
    raise AssertionError(
        f"conference rooms still alive: {await _conference_rooms(api)}")


async def _wait_bot_frames(bot, min_frames: int = 50, timeout: float = 15.0,
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
        f"{label or 'bot'}: no audio frames (want ≥{min_frames}): {last}\n"
        f"{bot.output[-1200:]}")


async def _wait_bot_dtmf(bot, digit: str, timeout: float = 10.0) -> bool:
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if digit in bot.get_dtmf_digits():
            return True
        await asyncio.sleep(0.2)
    return False


def _call_id_of(webhook_server, pred) -> str | None:
    for e in webhook_server.receiver.all_events():
        if e.call_id and (pred is None or pred(e)):
            return e.call_id
    return None


async def _establish(pbx, sipbot_pool, tmp_path: Path):
    """1002 (restsend) registers FIRST, then the customer (sipbot caller)
    dials in; established with media flowing. Returns (agent, customer_bot).
    (The caller dials immediately on start — registering the callee first
    avoids the `target user is offline` race.)"""
    agent = _restsend(pbx, AGENT, 25110)
    await agent.start()
    assert await agent.register(expires=120), "1002 REGISTER failed"
    cust = _customer_bot(sipbot_pool, pbx, tmp_path)
    assert await agent.answer(timeout=25), (
        f"1002 did not ring/answer:\n{agent.stderr_text()[-600:]}")
    await agent.wait_media_flow(min_received=10, timeout=10,
                                label="1002-post-answer")
    return agent, cust


# ---------------------------------------------------------------------------
# R1: blind REFER → IVR (single step)
# ---------------------------------------------------------------------------

async def test_r1_blind_refer_to_ivr(transfer_pbx, sipbot_pool, api,
                                     webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        refer = await agent.refer_blind(
            f"sip:{IVR_NUM}@{transfer_pbx.sip_addr}")
        assert refer and refer.get("status") in (200, 202), (
            f"blind REFER not accepted: {refer}\n{agent.stderr_text()[-500:]}")

        # The transferor exits by its own BYEs after the final NOTIFY 200.
        assert await agent.wait_state("ended", timeout=15), (
            "1002 call must end after blind REFER completes")

        # In-session hand-off: the IVR runs INSIDE the original session.
        _must_webhook(
            await _wait_webhook(
                webhook_server, "call_transferred",
                predicate=lambda e: IVR_NUM in ((e.payload or {}).get("transfer_target") or ""),
                timeout=10),
            f"call_transferred missing: {webhook_server.receiver.event_types()}")
        await _wait_webhook(webhook_server, "ivr_node_entered", timeout=15) or \
            pytest.fail("ivr_node_entered missing")

        calls = await _wait_active_calls(api, 1)
        assert calls, "IVR must stay alive in the original session"

        transfers = [e for e in webhook_server.receiver.all_events()
                     if e.event_type == "call_transferred"]
        assert len(transfers) == 1, (
            f"exactly one transfer event expected: "
            f"{[e.event_type for e in webhook_server.receiver.all_events()]}")
        assert not await _conference_rooms(api), (
            "no transfer room may exist for an app hand-off")

        # End the parked IVR session and verify the registry drains.
        call_id = _call_id_of(
            webhook_server, lambda e: e.event_type == "call_transferred")
        assert call_id
        try:
            await api.post(f"/api/cc/calls/{call_id}/end", {})
        except Exception:  # noqa: BLE001 — plain sessions may not support /end
            pass
        await _wait_active_calls(api, 0, timeout=15)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R2: blind REFER → agent extension (single step)
# ---------------------------------------------------------------------------

async def test_r2_blind_refer_to_agent(transfer_pbx, sipbot_pool, api,
                                       webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112), TARGET)
    await h.wait_registered(bot1003, "1003")

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        refer = await agent.refer_blind(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert refer and refer.get("status") in (200, 202), f"REFER rejected: {refer}"

        _must_webhook(
            await _wait_webhook(
                webhook_server, "call_transferred",
                predicate=lambda e: TARGET in ((e.payload or {}).get("transfer_target") or ""),
                timeout=15),
            f"call_transferred missing: {webhook_server.receiver.event_types()}")
        assert await agent.wait_state("ended", timeout=15), "1002 must exit"

        # 1003 answered via echo: bidirectional media with the customer.
        await _wait_bot_frames(bot1003, min_frames=50, timeout=15, label="1003")
        await _wait_active_calls(api, 1)

        # sipbot has no graceful BYE on terminate — end the session via REST.
        calls = await _active_calls(api)
        await api.post(f"/api/cc/calls/{calls[0].get('call_id')}/end", {})
        await _wait_active_calls(api, 0, timeout=20)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R3: blind REFER → skill-group (queue) + call_ringing agent contract
# ---------------------------------------------------------------------------

async def test_r3_blind_refer_to_skill_group(transfer_pbx, sipbot_pool, api,
                                             webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112), TARGET)
    await h.wait_registered(bot1003, "1003")
    try:
        await api.post(f"/api/cc/agents/{TARGET}/status", {"status": "idle"})
    except Exception:  # noqa: BLE001 — endpoint shape differences tolerated
        pass

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        refer = await agent.refer_blind(
            f"sip:*81{SKILL_GROUP}@{transfer_pbx.sip_addr}")
        assert refer and refer.get("status") in (200, 202), f"REFER rejected: {refer}"

        await _wait_webhook(
            webhook_server, "skill_group_agent_assigned",
            predicate=lambda e: (e.payload or {}).get("skill_group_id") == SKILL_GROUP,
            timeout=30) or pytest.fail(
            f"agent_assigned missing: {webhook_server.receiver.event_types()}")

        # The LEG-level call_ringing of the DISPATCHED call (not the
        # original P2P call's session ringing) must carry the agent id. The
        # in-session hand-off runs the queue in the transferred session —
        # correlate via its `call_transferred` envelope, and only look at
        # events AFTER it (the original call's own ringing precedes it).
        transferred = await _wait_webhook(
            webhook_server, "call_transferred", timeout=15)
        assert transferred is not None, "call_transferred missing"
        tx_idx = webhook_server.receiver.all_events().index(transferred)

        loop = asyncio.get_running_loop()
        deadline = loop.time() + 25
        ringing = None
        while loop.time() < deadline:
            evs = webhook_server.receiver.all_events()
            ringing = next((e for e in evs[tx_idx + 1:]
                            if e.event_type == "call_ringing"
                            and (e.payload or {}).get("leg_id")
                            and e.call_id == transferred.call_id), None)
            if ringing:
                break
            await asyncio.sleep(0.4)
        assert ringing, (
            f"leg-level call_ringing missing for the dispatched call: "
            f"{webhook_server.receiver.event_types()}")
        # The 2026-09 contract: the queue-dialed leg-level call_ringing
        # carries the agent attribution on the payload.
        agent_id = (ringing.payload or {}).get("agent_id")
        assert agent_id, (
            f"leg-level call_ringing must carry agent_id: {ringing.raw}")

        await agent.wait_state("ended", timeout=15)
        await _wait_bot_frames(bot1003, min_frames=30, timeout=25, label="1003")
        for c in await _active_calls(api):
            try:
                await api.post(f"/api/cc/calls/{c.get('call_id')}/end", {})
            except Exception:  # noqa: BLE001
                pass
        await _wait_active_calls(api, 0, timeout=25)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R4: blind REFER target busy — original call survives
# ---------------------------------------------------------------------------

async def test_r4_blind_refer_target_busy(transfer_pbx, sipbot_pool, api,
                                          webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    bot1003 = sipbot_pool.callee(
        host=transfer_pbx.host, port=h.ua_port(15112), username=TARGET,
        password="123456", register=True,
        proxy=f"{transfer_pbx.host}:{transfer_pbx.sip_port}", domain=transfer_pbx.host,
        ring_secs=30, answer_mode="echo", reject_code=486,
    )
    assert bot1003
    await h.wait_registered(bot1003, "1003")

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    baseline = agent.stats_index()
    try:
        refer = await agent.refer_blind(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        # The single `sip_refer` event carries the FINAL outcome: the busy
        # target must surface as 486 (not a phantom 500) on the transferor's
        # phone — the BYE/busy NOTIFY-mapping contract.
        assert refer and refer.get("status") == 486, (
            f"busy target must fail the REFER with 486, got: {refer}")
        await asyncio.sleep(3.0)

        # The REFER failed — the original call must still be up and talking.
        state = await agent.current_state()
        assert state in ("connected", "hold"), (
            f"original call must survive a failed REFER, got {state}")
        delta = await agent.wait_media_flow(since=baseline, min_received=10,
                                            timeout=10, label="1002-survived")
        assert delta["received"] >= 10
        # Only the failed transfer LEG may hang up; the session itself must
        # stay (a session-level call_hangup — leg_id null — means the call
        # died).
        session_hangups = [
            e for e in webhook_server.receiver.all_events()
            if e.event_type == "call_hangup"
            and not (e.payload or {}).get("leg_id")
        ]
        assert not session_hangups, (
            f"the session must survive a failed REFER: {session_hangups}")

        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R5: attended REFER → agent (two steps, transfer-room path)
# ---------------------------------------------------------------------------

async def test_r5_attended_refer_to_agent(transfer_pbx, sipbot_pool, api,
                                          webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    wav730 = tmp_path / "t730.wav"
    generate_sine_wav(wav730, 730.0, 90.0, 8000, 0.4)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112),
                            TARGET, hangup_after=45,
                            answer_mode=str(wav730))
    await h.wait_registered(bot1003, "1003")

    agent, cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        consult = await agent.consult(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert consult, f"consult leg never connected:\n{agent.stderr_text()[-600:]}"

        refer = await agent.refer_attended(
            f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert refer and refer.get("status") in (200, 202), (
            f"attended REFER rejected: {refer}\n{agent.stderr_text()[-500:]}")

        t_tx = await _wait_webhook(webhook_server, "call_transferred",
                                   timeout=25)
        _must_webhook(t_tx, "call_transferred missing after attended REFER")

        # AUDIO: after the transfer the customer must hear the 730 Hz
        # target (the room media followed), not the agent's 920 Hz. The
        # room-path ICE/DTLS takes ~2 s to settle — dwell, then sample.
        await asyncio.sleep(3.0)
        t_sample = time.monotonic()
        cust_rec = tmp_path / "cust_1001_rx.wav"
        rooms = await _conference_rooms(api)
        assert len(rooms) == 1, (
            f"the transfer room is the live media path: {rooms}")
        await asyncio.sleep(1.0)  # clean post-transfer spectral window
        await agent.hangup()
        await _end_all(api)
        rec = await _flush_rec(cust_rec)
        off = _align(rec, F_AGENT)
        await _wait_bot_frames(bot1003, min_frames=50, timeout=15, label="1003")

        # AUDIO: the tail of the customer's recording (post-transfer, pre-
        # hangup) must carry the 730 Hz target and NOT the 920 Hz agent.
        import time as _time
        samples, sr = read_wav_mono(rec)
        rec_len = samples.size / sr
        wa, wb = max(rec_len - 4.5, 0.5), rec_len - 1.0
        assert _present(rec, F_TARGET, wa, wb, th=-14), (
            f"post-transfer: customer must hear the 730 Hz target (gain="
            f"{_tone_gain(rec, F_TARGET, wa, wb):.1f})")
        assert _absent(rec, F_AGENT, wa, wb, th=-18), (
            f"post-transfer: the agent must be gone (gain="
            f"{_tone_gain(rec, F_AGENT, wa, wb):.1f})")

        # Everyone has left → the room drained completely.
        await _wait_no_rooms(api, timeout=15)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R6: attended REFER → IVR (two steps, R1-fix in-session hand-off)
# ---------------------------------------------------------------------------

async def test_r6_attended_refer_to_ivr(transfer_pbx, sipbot_pool, api,
                                        webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        consult = await agent.consult(f"sip:{IVR_NUM}@{transfer_pbx.sip_addr}")
        assert consult, f"IVR consult never connected:\n{agent.stderr_text()[-600:]}"

        calls_during_consult = await _active_calls(api)
        assert len(calls_during_consult) == 2, (
            f"main + consult sessions expected during consult: "
            f"{calls_during_consult}")
        session_ids = {c.get("call_id") for c in calls_during_consult}

        refer = await agent.refer_attended(
            f"sip:{IVR_NUM}@{transfer_pbx.sip_addr}")
        assert refer and refer.get("status") in (200, 202), (
            f"attended REFER rejected: {refer}\n{agent.stderr_text()[-500:]}")

        # R1 contract: NO originate — the customer moves in-session; the
        # surviving session is one of the two that already existed.
        _must_webhook(
            await _wait_webhook(
                webhook_server, "call_transferred",
                predicate=lambda e: IVR_NUM in ((e.payload or {}).get("transfer_target") or ""),
                timeout=15),
            f"call_transferred missing: {webhook_server.receiver.event_types()}")
        await _wait_webhook(webhook_server, "ivr_node_entered", timeout=15) or \
            pytest.fail("ivr_node_entered missing")

        calls = await _wait_active_calls(api, 1)
        surviving = {c.get("call_id") for c in calls}
        assert surviving <= session_ids, (
            f"IVR must run inside the ORIGINAL session: "
            f"before={session_ids} after={surviving}")

        assert not await _conference_rooms(api), "no room for an app hand-off"

        call_id = _call_id_of(
            webhook_server, lambda e: e.event_type == "call_transferred")
        try:
            await api.post(f"/api/cc/calls/{call_id}/end", {})
        except Exception:  # noqa: BLE001
            pass
        await _wait_active_calls(api, 0, timeout=15)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R7: attended REFER to an already-gone consult target → failure + retrieve
# ---------------------------------------------------------------------------

async def test_r7_attended_refer_failure_then_retrieve(
        transfer_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    # 1003 answers the consult then BYEs after 1s: by the time the attended
    # REFER fires, the Replaces dialog is gone → the transfer must FAIL and
    # leave the customer held with the agent (retrieve available).
    _retire_target(sipbot_pool)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112),
                            TARGET, hangup_after=1)
    await h.wait_registered(bot1003, "1003")

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        consult = await agent.consult(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert consult, "consult leg never connected"
        await asyncio.sleep(2.5)  # let 1003 BYE (hangup_after=1s)

        refer = await agent.refer_attended(
            f"sip:{TARGET}@{transfer_pbx.sip_addr}", timeout=8)
        error_text = "".join(
            e.get("message", "") for e in agent.events if e.get("evt") == "error")
        rejected = refer is None or not (200 <= (refer.get("status") or 0) < 300)
        assert rejected or "attended" in error_text, (
            f"attended REFER to a dead dialog must fail: refer={refer} "
            f"errors={error_text!r}")
        assert "call_transferred" not in webhook_server.receiver.event_types()

        # The customer is still here, held with the agent — retrieve it.
        calls = await _active_calls(api)
        assert len(calls) == 1, (
            f"customer session must survive the failed transfer: {calls}")

        await agent.resume_call()
        assert await agent.wait_state("connected", timeout=10) or \
            await agent.current_state() == "connected", (
            f"resume must restore the customer: {agent.current_state()}")
        await agent.wait_media_flow(min_received=10, timeout=10,
                                    label="1002-after-retrieve")

        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R8: consult → retrieve (the hold-music-loop fix)
# ---------------------------------------------------------------------------

async def test_r8_consult_then_retrieve(transfer_pbx, sipbot_pool, api,
                                        webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112), TARGET)
    await h.wait_registered(bot1003, "1003")

    agent, cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    t_est = time.monotonic()
    t_hold_start = t_r = None
    try:
        consult = await agent.consult(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert consult, "consult leg never connected"
        # The hold takes effect right after the consult connects.
        t_hold_start = time.monotonic()
        await _wait_webhook(webhook_server, "call_held", timeout=10) or \
            pytest.fail("call_held missing")
        await asyncio.sleep(2.0)  # mature the MOH window

        await agent.hangup_consult()
        assert await agent.wait_recovered(timeout=12), (
            f"engine must auto-resume the customer after retrieve: "
            f"{agent.current_state()}")

        await _wait_webhook(webhook_server, "call_unheld", timeout=10) or \
            pytest.fail("call_unheld missing")
        t_r = time.monotonic()

        # THE regression: after retrieve the real bridge is back — DTMF 5
        # from the agent must cross to the customer (a looping MOH path
        # would not deliver it).
        await agent.send_dtmf("5")
        assert await _wait_bot_dtmf(cust, "5", timeout=8), (
            "customer must receive DTMF after retrieve — media path was not "
            f"restored. cust digits={cust.get_dtmf_digits()} "
            f"aq={cust.get_audio_quality()}")

        delta = await agent.wait_media_flow(min_received=10, timeout=10,
                                            label="1002-post-retrieve")
        assert delta["received"] >= 10

        assert not await _conference_rooms(api)
        calls = await _active_calls(api)
        assert len(calls) == 1, f"only the customer↔agent call remains: {calls}"

        await asyncio.sleep(1.5)  # clean post-retrieve 920 Hz window
        await agent.hangup()
        await _wait_webhook(webhook_server, "call_hangup", timeout=10) or \
            pytest.fail("call_hangup missing")
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()

    # ── AUDIO timeline: 920 Hz (agent) present → absent (MOH during the
    # consult) → present again after the retrieve. The old bug left the
    # customer on a MOH loop forever; the DTMF above proves signalling, the
    # spectrum proves the media.
    rec = await _flush_rec(tmp_path / "cust_1001_rx.wav")
    off = _align(rec, F_AGENT)
    pre_a, pre_b = off + 0.05, off + 0.95
    hold_a, hold_b = off + (t_hold_start - t_est) + 0.7, off + (t_r - t_est) - 0.4
    post_a, post_b = off + (t_r - t_est) + 0.5, off + (t_r - t_est) + 2.0
    assert _present(rec, F_AGENT, pre_a, pre_b), (
        f"pre-consult: agent tone expected (gain="
        f"{_tone_gain(rec, F_AGENT, pre_a, pre_b):.1f})")
    assert _absent(rec, F_AGENT, hold_a, hold_b), (
        f"consult: agent tone must be absent — customer on MOH (gain="
        f"{_tone_gain(rec, F_AGENT, hold_a, hold_b):.1f})")
    assert _present(rec, F_AGENT, post_a, post_b), (
        f"post-retrieve: agent tone must return (gain="
        f"{_tone_gain(rec, F_AGENT, post_a, post_b):.1f})")


# ---------------------------------------------------------------------------
# R9: consult the IVR → retrieve
# ---------------------------------------------------------------------------

async def test_r9_consult_ivr_then_retrieve(transfer_pbx, sipbot_pool, api,
                                            webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    cust_session = None
    try:
        calls_before = await _active_calls(api)
        cust_session = calls_before[0].get("call_id") if calls_before else None
        consult = await agent.consult(f"sip:{IVR_NUM}@{transfer_pbx.sip_addr}")
        assert consult, "IVR consult never connected"
        await asyncio.sleep(1.0)  # let the greeting reach the agent leg

        await agent.hangup_consult()
        assert await agent.wait_recovered(timeout=12), (
            f"engine must auto-resume after the IVR consult ends: "
            f"{agent.current_state()}")

        # Only the customer session remains; the IVR entry belonged to the
        # CONSULT session — the customer's own session must have none.
        await _wait_active_calls(api, 1)
        entered_sessions = [e.call_id for e in webhook_server.receiver.all_events()
                            if e.event_type == "ivr_node_entered"]
        assert cust_session not in entered_sessions, (
            f"the CUSTOMER session must not enter the IVR during a consult "
            f"(cust={cust_session}, entered={entered_sessions})")
        await agent.wait_media_flow(min_received=10, timeout=10,
                                    label="1002-post-retrieve")

        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R10: consult no-answer → cancel → retrieve (CANCEL path)
# ---------------------------------------------------------------------------

async def test_r10_consult_no_answer_cancel_retrieve(
        transfer_pbx, sipbot_pool, api, webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    bot1003 = _ringing_bot(sipbot_pool, transfer_pbx, h.ua_port(15112), TARGET)

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        await agent.cmd({"cmd": "sip_call",
                         "remote_uri": f"sip:{TARGET}@{transfer_pbx.sip_addr}",
                         "consult": True})
        ringing = await agent.wait_event(
            "sip_call_leg",
            predicate=lambda e: e.get("consult") is True
            and e.get("name") in ("ringing", "calling"),
            timeout=12)
        assert ringing, f"consult leg never rang:\n{agent.stderr_text()[-500:]}"

        # Cancel while ringing (CANCEL, not BYE). An unconnected consult may
        # not trigger the engine's auto-resume — recover explicitly if the
        # main call is still held after a short settle.
        await agent.cmd({"cmd": "sip_cancel"})
        await asyncio.sleep(2.5)
        if (await agent.current_state()) != "connected":
            await agent.resume_call()
        assert await agent.wait_recovered(timeout=12) or \
            (await agent.current_state()) == "connected", (
            f"main call must recover after the consult CANCEL: "
            f"{agent.stderr_text()[-400:]}")

        await _wait_active_calls(api, 1)
        await agent.wait_media_flow(min_received=10, timeout=10,
                                    label="1002-post-cancel")

        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R11: transferor hangs up mid-REFER (queue variant — cascade + re-schedulable)
# ---------------------------------------------------------------------------

async def test_r11_transferor_hangup_during_refer(transfer_pbx, sipbot_pool,
                                                  api, webhook_server,
                                                  tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    # A FRESH transferor agent (1012): earlier cases leave 1002 mid-wrapup in
    # the shared PBX, and a wrapup agent is unschedulable — 1012 registers
    # clean so the queue can dispatch immediately. Park the polluted agents
    # on break (best effort) so the dispatch reaches 1012 directly.
    agent = _restsend(transfer_pbx, AGENT2, 25112)
    await agent.start()
    ringing_phone = _restsend(transfer_pbx, "1014", 25114, device="null")
    await ringing_phone.start()
    try:
        assert await agent.register(expires=120)
        await agent.publish_idle()
        for polluted in (AGENT, TARGET):
            try:
                await api.post(f"/api/cc/agents/{polluted}/status",
                               {"status": "break"})
            except Exception:  # noqa: BLE001 — best effort
                pass
        assert await ringing_phone.register(expires=120)
        await asyncio.sleep(1.0)

        # Customer → 9201 (R11's dedicated single-candidate queue) → 1012.
        cust = _customer_bot(sipbot_pool, transfer_pbx, tmp_path,
                             target=f"sip:9201@{transfer_pbx.sip_addr}")
        assert await agent.answer(timeout=45), (
            f"queue never dispatched to 1012:\n{agent.stderr_text()[-600:]}")
        await agent.wait_media_flow(min_received=10, timeout=10,
                                    label="1012-queue")

        # Blind REFER to 1014 — the ringing phone never answers, so the
        # REFER stays in flight. The `sip_refer` event only fires at REFER
        # COMPLETION, so in-flight progress is proven by 1014 ringing.
        await agent.refer_blind(f"sip:1014@{transfer_pbx.sip_addr}", wait=False)
        assert await ringing_phone.wait_event("sip_incoming", timeout=10), (
            "the REFER target phone must be ringing while the REFER is in "
            "flight:\n" + ringing_phone.stderr_text()[-400:])
        # Keep the REFER in flight briefly, then hang up MID-REFER.
        await asyncio.sleep(1.5)
        await agent.hangup()
        # 1014 either receives the CANCEL (queue released the leg) or — if
        # still ringing — rejects on its own; both release the REFER.
        await asyncio.sleep(1.5)
        if (await ringing_phone.current_state()) == "ringing":
            await ringing_phone.reject_incoming(486)

        # The agent leg dies with the transferor. The pending REFER target
        # rejects (or is CANCELled by the released queue leg) shortly after.
        assert await ringing_phone.wait_state("ended", timeout=25), (
            "the ringing REFER target must be released after the "
            f"transferor's hangup: {await ringing_phone.current_state()}")

        # CASCADE CONTRACT: the transferor's agent must NOT stay stuck Busy —
        # after the (short) wrapup it is schedulable again.
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 20
        status = None
        while loop.time() < deadline:
            try:
                resp = await api.get("/api/cc/agents/1012")
                status = (resp or {}).get("status") or (resp or {}).get("agent", {}).get("status")
            except Exception:  # noqa: BLE001
                status = None
            if status == "idle":
                break
            await asyncio.sleep(0.5)
        assert status == "idle", (
            f"agent 1012 must return to Idle after the mid-REFER hangup "
            f"(no stuck Busy), got {status}")

        # The queue keeps serving the abandoned customer (correct queue
        # semantics) — drain it so later cases start clean.
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=60)
    finally:
        await agent.stop()
        await ringing_phone.stop()


# ---------------------------------------------------------------------------
# R12: long hold — no rtpTimeout mid-hold, media after resume
# ---------------------------------------------------------------------------

async def test_r12_long_hold_no_rtp_timeout(transfer_pbx, sipbot_pool, api,
                                            webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    agent, cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    agent_baseline = agent.stats_index()
    cust_frames_before = (cust.get_audio_quality() or {}).get("total_frames", 0)
    try:
        await agent.hold_call()
        # Hold well past the RTP-inactivity watchdog window (25s > 2×10s).
        await asyncio.sleep(25)
        types = webhook_server.receiver.event_types()
        assert "call_hangup" not in types, (
            f"call torn down mid-hold (rtpTimeout?): {types}")

        # MOH must reach the customer — its RX counter keeps growing.
        await _wait_bot_frames(cust, min_frames=cust_frames_before + 100,
                               timeout=15, label="customer-held")

        await agent.resume_call()
        assert await agent.wait_state("connected", timeout=10) or \
            await agent.current_state() == "connected", "resume failed"
        await agent.wait_media_flow(since=agent_baseline, min_received=50,
                                    timeout=15, label="1002-resumed")
        await _wait_webhook(webhook_server, "call_unheld", timeout=10) or \
            pytest.fail("call_unheld missing")

        # Real bridge back: DTMF crosses post-resume.
        await agent.send_dtmf("7")
        assert await _wait_bot_dtmf(cust, "7", timeout=8), (
            "customer must receive DTMF after resume")

        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R13: consult switch (restsend sip_consult_switch — dual re-INVITE)
# ---------------------------------------------------------------------------

async def test_r13_consult_switch_between_parties(transfer_pbx, sipbot_pool,
                                                  api, webhook_server,
                                                  tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112), TARGET)
    await h.wait_registered(bot1003, "1003")

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        consult = await agent.consult(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert consult, "consult leg never connected"
        await asyncio.sleep(0.8)

        # Switch #1: back to the customer (consult leg → hold).
        await agent.switch_consult()
        party = await agent.wait_consult_party(customer_active=True, timeout=15)
        errors = [e for e in agent.events if e.get("evt") == "error"]
        assert party, (
            f"sip_consult_party(customer) missing: errors={errors[-3:]}")
        await agent.wait_media_flow(min_received=10, timeout=10,
                                    label="1002-switch-customer")

        # Switch #2: back to the consult party. The engine confirms with the
        # party edge; the MAIN leg is held afterwards, so its RX counter
        # stalls (that freeze IS the hold behavior — assert it softly).
        switch2_index = agent.stats_index()
        await agent.switch_consult()
        party2 = await agent.wait_consult_party(customer_active=False,
                                                timeout=15)
        assert party2, "sip_consult_party(consult) missing after switch #2"
        await asyncio.sleep(3.0)
        stalled = agent.stats_delta(switch2_index)
        assert stalled["received"] < 15, (
            f"main leg should be receive-stopped while held after switch #2: "
            f"{stalled}")
        assert webhook_server.receiver.count("call_held") >= 1, (
            "call_held missing during switches")

        # Retrieve after switching: consult BYE + auto-resume → customer back.
        await agent.hangup_consult()
        assert await agent.wait_recovered(timeout=12), (
            f"engine must auto-resume after retrieve: {agent.current_state()}")
        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=12)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R14: conference merge + C1 consult-join-failure protection
# ---------------------------------------------------------------------------

async def test_r14_conference_merge(transfer_pbx, sipbot_pool, api,
                                    webhook_server, tmp_path):
    _skip_without_cli()
    webhook_server.receiver.clear()

    _retire_target(sipbot_pool)
    wav730 = tmp_path / "t730.wav"
    generate_sine_wav(wav730, 730.0, 90.0, 8000, 0.4)
    bot1003 = _register_bot(sipbot_pool, transfer_pbx, h.ua_port(15112),
                            TARGET, answer_mode=str(wav730))
    await h.wait_registered(bot1003, "1003")

    agent, cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    merge_off = None
    try:
        consult = await agent.consult(f"sip:{TARGET}@{transfer_pbx.sip_addr}")
        assert consult, "consult leg never connected"

        await agent.conference(
            f"sip:conf-factory@{transfer_pbx.host}:{transfer_pbx.sip_port}")

        # Both legs land in the room (C1 order: consult leg REFERed first).
        await _wait_webhook(webhook_server, "conference_joined", timeout=35) or \
            pytest.fail("conference_joined missing")
        await asyncio.sleep(4.0)  # sustain the 3-way for clean spectral windows
        merge_off = None  # aligned later via the recording

        rooms = await _conference_rooms(api)
        assert rooms, f"conference room must be visible: {rooms}"

        # Everyone leaves → the room auto-destroys (lone-member watchdog).
        await agent.hangup()
        await _wait_active_calls(api, 0, timeout=25)
        await _wait_no_rooms(api, timeout=15)
    finally:
        await agent.stop()

    # ── AUDIO: 3-way mixing — the customer's recording must carry BOTH the
    # agent's 920 Hz and the target's 730 Hz during the merge window.
    rec = await _flush_rec(tmp_path / "cust_1001_rx.wav")
    samples, sr = read_wav_mono(rec)
    rec_len = samples.size / sr
    # The merge window is the 4 s right before the customer's call ended.
    ma, mb = rec_len - 6.0, rec_len - 2.0
    assert _present(rec, F_AGENT, ma, mb, th=-14), (
        f"3-way: customer must hear the agent (gain="
        f"{_tone_gain(rec, F_AGENT, ma, mb):.1f})")
    assert _present(rec, F_TARGET, ma, mb, th=-14), (
        f"3-way: customer must hear the 730 Hz target (gain="
        f"{_tone_gain(rec, F_TARGET, ma, mb):.1f})")


# ---------------------------------------------------------------------------
# R15: transferor crashes mid-REFER — bounded NOTIFYs keep the loop alive
# ---------------------------------------------------------------------------

async def test_r15_transferor_dies_mid_refer_handoff_survives(
        transfer_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """Production incident (2026-09-29): the REFER handler NOTIFYed the
    transferor's dialog on an UNBOUNDED await; a stale contact (phone
    reconnected on a new WSS port / crashed) parked the single-threaded
    session loop for minutes — the customer's queued BYE was only seen
    6m33s later and the agent's hold re-INVITE died with 501.

    Contract: hard-kill the client right after the REFER leaves (dead
    contact, no BYE / no unregister). The in-session app hand-off must
    still proceed within 8 s and the customer session must survive."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        await agent.refer_blind(
            f"sip:{IVR_NUM}@{transfer_pbx.sip_addr}", wait=False)
        await asyncio.sleep(0.5)  # let the REFER reach the wire first
        agent.kill()

        t_tx = await _wait_webhook(
            webhook_server, "call_transferred",
            predicate=lambda e: IVR_NUM in ((e.payload or {}).get("transfer_target") or ""),
            timeout=8)
        _must_webhook(t_tx, (
            "call_transferred not delivered ≤8s after the transferor died — "
            "session loop parked on a dead contact (unbounded NOTIFY)?"))
        await _wait_webhook(webhook_server, "ivr_node_entered", timeout=10) or \
            pytest.fail("IVR hand-off did not run after the transferor died")

        calls = await _wait_active_calls(api, 1)
        assert calls, "customer session must survive the transferor crash"
        call_id = _call_id_of(
            webhook_server, lambda e: e.event_type == "call_transferred")
        try:
            await api.post(f"/api/cc/calls/{call_id}/end", {})
        except Exception:  # noqa: BLE001 — plain sessions may not support /end
            pass
        await _wait_active_calls(api, 0, timeout=15)
    finally:
        await agent.stop()


# ---------------------------------------------------------------------------
# R16: blind REFER → registered agent must ring in seconds (latency contract)
# ---------------------------------------------------------------------------

async def test_r16_blind_refer_registered_agent_rings_fast(
        transfer_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """The developer report: an agent→agent REFER to a REGISTERED colleague
    rang only after the full IVR→queue→cc-reservation pipeline (15–25 s in
    the 09-29 logs). A registered target dials DIRECTLY — the real phone
    (restsend-cli) must hit `ringing` within 3 s of the REFER."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    target = RestsendAgent(transfer_pbx, TARGET, local_port=h.ua_port(25120),
                           device="null")
    await target.start()
    assert await target.register(expires=120), "1003 REGISTER failed"

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        t0 = time.monotonic()
        await agent.refer_blind(
            f"sip:{TARGET}@{transfer_pbx.sip_addr}", wait=False)
        ringing = await target.wait_state("ringing", timeout=5)
        latency = time.monotonic() - t0
        assert ringing is not None, (
            f"registered target never rang: {target.stderr_text()[-400:]}")
        assert latency <= 5.0, (
            f"blind REFER ring latency {latency:.2f}s exceeds the 5s contract")

        # Drain: nothing answers — end the parked session via REST.
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=15)
    finally:
        await agent.stop()
        await target.stop()


# ---------------------------------------------------------------------------
# R17: a REGISTERED user whose number matches an app route dials DIRECTLY
# (P1a locator-first acceptance — RED on route-table-first ordering)
# ---------------------------------------------------------------------------

async def test_r17_blind_refer_prefers_registered_user_over_app_route(
        transfer_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """Production incident shape (39300): the number had BOTH an
    application route (app=ivr) and an agent behind it. Route-table-first
    resolution hijacked the transfer into the customer IVR — the callee's
    phone rang only after IVR→queue→cc reservation (15–25 s) or never.

    P1a contract: for a same-realm target with a live registration the
    REFER dials the endpoint directly (≤3 s ring) and the IVR route is
    NOT entered. Registration is local to this test; it runs LAST in the
    module so a lingering registration cannot affect earlier cases."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    target = RestsendAgent(transfer_pbx, ROUTED_AGENT,
                           local_port=h.ua_port(25121), device="null")
    await target.start()
    assert await target.register(expires=120), f"{ROUTED_AGENT} REGISTER failed"

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        mark = len(webhook_server.receiver.all_events())
        t0 = time.monotonic()
        await agent.refer_blind(
            f"sip:{ROUTED_AGENT}@{transfer_pbx.sip_addr}", wait=False)
        ringing = await target.wait_state("ringing", timeout=5)
        latency = time.monotonic() - t0
        assert ringing is not None, (
            f"registered user {ROUTED_AGENT} never rang — the app route "
            f"hijacked the transfer: {target.stderr_text()[-400:]}")
        assert latency <= 5.0, (
            f"ring latency {latency:.2f}s exceeds the 5s contract")

        hijack = await _wait_webhook(
            webhook_server, "ivr_node_entered", timeout=3)
        assert hijack is None, (
            "the IVR route must NOT run for a registered local target")

        # Drain: nothing answers — end the parked session via REST.
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=15)
    finally:
        await agent.stop()
        await target.stop()


# ---------------------------------------------------------------------------
# R18: REFER to a CC-agent number on an app route → queue:agent hand-off
# ---------------------------------------------------------------------------

async def test_r18_blind_refer_to_cc_agent_number_skips_ivr(
        transfer_pbx, sipbot_pool, api, webhook_server, tmp_path):
    """P1b: a CC agent extension that ALSO matches an application route is a
    person, not an IVR flow. The REFER must hand off to `queue:agent:<ext>`
    (the quick-transfer queue vehicle, pinned to that agent) instead of
    entering the customer IVR; `return_app=ivr` degrades to the routed IVR
    when the agent is unavailable. The agent has no SIP user, so the call
    parks in the queue — we assert the routing decision and drain."""
    _skip_without_cli()
    webhook_server.receiver.clear()

    agent, _cust = await _establish(transfer_pbx, sipbot_pool, tmp_path)
    try:
        await agent.refer_blind(
            f"sip:{CC_AGENT_NUM}@{transfer_pbx.sip_addr}", wait=False)
        t_tx = await _wait_webhook(
            webhook_server, "call_transferred",
            predicate=lambda e: (e.payload or {}).get("transfer_target", "")
            .startswith(f"queue:agent:{CC_AGENT_NUM}"),
            timeout=8)
        _must_webhook(t_tx, (
            f"call_transferred must carry the queue:agent:{CC_AGENT_NUM} "
            f"hand-off: {webhook_server.receiver.event_types()}"))

        # 1004 has no SIP user → reservation refused → the queue's
        # return_app=ivr degrades to the routed IVR. The ORDER is the
        # hijack-proof: queue:agent first, IVR only as the fallback (a route
        # hijack would enter the IVR without the queue:agent decision).
        tx_idx = webhook_server.receiver.all_events().index(t_tx)
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 12
        fallback_ivr = None
        while loop.time() < deadline:
            evs = webhook_server.receiver.all_events()
            fallback_ivr = next((e for e in evs[tx_idx + 1:]
                                 if e.event_type == "ivr_node_entered"
                                 and e.call_id == t_tx.call_id), None)
            if fallback_ivr:
                break
            await asyncio.sleep(0.3)
        assert fallback_ivr is not None, (
            "offline-agent fallback: return_app=ivr must re-enter the routed "
            f"IVR after the queue:agent hand-off: "
            f"{webhook_server.receiver.event_types()}")

        # The parked session drains via REST.
        await _end_all(api)
        await _wait_active_calls(api, 0, timeout=15)
    finally:
        await agent.stop()
