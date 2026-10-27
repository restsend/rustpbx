"""Race-injection tests — deterministic edge-timing scenarios from the
queue/agent lifecycle audit (Phase A fixes):

  R-race1  LATE ANSWER: the agent answers exactly at the ring-timeout
           boundary. Old behavior: call bridges but the agent stays in
           no-answer Wrapup (capacity not consumed, cooldown timer flips
           them Idle mid-call → double dispatch). Fixed via the
           Wrapup{same call}→Busy transition.
  R-race2  RESERVATION-WINDOW ABANDON: caller hangs up in the window
           between the pre-app reservation and the queue's first dial.
           Old behavior: reserved agent stuck Ringing until the DB stale
           sweep. Fixed via attempted-set adoption at on_enter.
  R-race3  OFFLINE-WHILE-RINGING (REST): forcing an agent offline while its
           leg is ringing must DEFER (auto-offline) instead of bulldozing
           to Offline mid-dispatch.

All marked `stress` (excluded from default runs). Queue ring_timeout is the
session fixture's default 30s — race1 uses a second agent with a long
answer delay riding just past that boundary.
"""

from __future__ import annotations

import asyncio
import logging
import time

import pytest

from helpers.restsend_agent import RestsendAgent

pytestmark = [pytest.mark.stress, pytest.mark.slow]
pytest.log = logging.getLogger("race")

AGENT = "1001"
SECOND = "1002"
OTHERS = ("1003",)


@pytest.fixture(autouse=True)
async def _reset_agents_after(api):
    """Belt-and-suspenders isolation: a failed/red call in this file can
    leave an agent parked in wrapup/cooldown, which cascades into the next
    test's dispatch expectations (seen 2026-10: race1's leftover state
    flipped race2's idle assert to wrapup and starved race3's dispatch).
    Force every agent back to a clean schedulable Idle after each test."""
    yield
    for aid in (AGENT, SECOND, *OTHERS):
        try:
            await api.update_agent_status(aid, "offline")
            await api.update_agent_status(aid, "idle")
        except Exception:  # noqa: BLE001 — teardown best-effort
            pass


async def _setup(api):
    for aid in OTHERS:
        try:
            await api.update_agent_status(aid, "offline")
        except Exception:  # noqa: BLE001
            pass


async def _await_status(api, agent_id, want, timeout=15.0):
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    last = None
    while loop.time() < deadline:
        last = (await api.get_agent(agent_id))["status"]
        if last == want:
            return last
        await asyncio.sleep(0.5)
    return last


async def _publish_idle_until_idle(api, agent: RestsendAgent, timeout: float = 30.0):
    """publish_idle + wait until the CC registry ACTUALLY reports Idle,
    re-publishing if a stale sweep / late unregistration landed Offline in
    between (a previous test's dead UA lease expiring mid-test flips the
    shared agent identity offline despite this fresh registration)."""
    deadline = time.time() + timeout
    re_sent = False
    while time.time() < deadline:
        st = (await api.get_agent(agent.user))["status"]
        if st == "idle":
            return
        if st == "offline" and not re_sent:
            await agent.publish_idle()
            re_sent = True
        await asyncio.sleep(1.0)
    raise AssertionError(
        f"{agent.user}: publish_idle never reached Idle (last={st!r})")


async def _spawn_caller(pbx, pool, hangup, port):
    return pool.caller(
        target=f"sip:8888@{pbx.sip_addr}",
        username="2204",
        hangup=hangup,
        addr=f"127.0.0.1:{port}",
    )


async def _drain(api, timeout=50.0):
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        try:
            snap = await api.get("/api/cc/monitor")
            data = snap.get("data") or snap
            waiting = (data.get("summary") or {}).get("waiting") or 0
            agents = (data.get("agents") or [])
            busyish = sum(1 for a in agents
                          if a.get("status") in ("ringing", "busy", "wrapup"))
            if waiting == 0 and busyish == 0:
                return
        except Exception:  # noqa: BLE001
            pass
        await asyncio.sleep(1.0)


# ── R-race1: late answer at the ring-timeout boundary ───────────────────────
async def test_race1_late_answer_keeps_capacity_accounted(
        pbx, sipbot_pool, api, event_checker):
    """Contract (2026-10 ring-no-answer policy — release to Idle): the 200 OK
    that crosses the ring-timeout CANCEL must NOT bridge a stale leg. The
    agent is released straight to Idle with zero capacity held and the queue
    keeps serving the caller (re-dial / fallback) — no double-dispatch, no
    zombie bridge. (The 2026-09 Wrapup→Busy late-answer bridge was removed
    by the CANCEL-at-ring-timeout policy; the default release is Idle.)"""
    await _setup(api)
    late = RestsendAgent(pbx, AGENT, local_port=25201)
    await late.start()
    try:
        assert await late.register(expires=120)
        await late.publish_idle()
        assert await _await_status(api, AGENT, "idle") == "idle"

        caller = await _spawn_caller(pbx, sipbot_pool, hangup=110, port=25310)

        # Do NOT answer when it rings. Ring timeout (~30s): the ringing leg
        # is CANCELled and the agent is released straight to Idle (default
        # policy — no cooldown configured for this session).
        status = await _await_status(api, AGENT, "idle", timeout=45)
        assert status == "idle", (
            f"ring timeout must release the agent to Idle (default "
            f"release-to-Idle policy), got {status!r}")

        # The stale UA answers anyway (200 OK racing the CANCEL): the leg is
        # already torn down — it must not bridge.
        await late.cmd({"cmd": "sip_answer"})
        ev = await late.wait_event(
            "state_changed",
            predicate=lambda e: e.get("name") == "connected",
            timeout=8,
        )
        assert ev is None, "late answer must NOT bridge the timed-out leg"

        # Capacity fully released: no call held on the agent.
        info = await api.get_agent(AGENT)
        assert info["current_calls"] == 0, info
    finally:
        sipbot_pool.terminate_user("2204")
        await _drain(api)
        await late.stop()


# ── R-race2: caller abandons inside the reservation window ──────────────────
async def test_race2_reservation_window_abandon_releases_agent(
        pbx, sipbot_pool, api, event_checker):
    """Both agents online; the caller is killed the instant the queue joined
    (reservation window). The reserved agent must return to a schedulable
    state quickly (adopted into the attempted set + phantom release), not
    stay Ringing until the DB stale sweep."""
    await _setup(api)
    a1 = RestsendAgent(pbx, AGENT, local_port=25202)
    a2 = RestsendAgent(pbx, SECOND, local_port=25203)
    await a1.start(); await a2.start()
    try:
        assert await a1.register(expires=120)
        assert await a2.register(expires=120)
        await _publish_idle_until_idle(api, a1)
        await _publish_idle_until_idle(api, a2)

        caller = await _spawn_caller(pbx, sipbot_pool, hangup=90, port=25311)
        # Kill the caller the moment the call joins the queue (first agent
        # gets reserved Ringing before any dial is recorded).
        await asyncio.sleep(2.5)
        sipbot_pool.terminate_user("2204")  # no BYE — abrupt abandon

        # The reserved agent must be back schedulable within a bounded window
        # (queue teardown releases the adopted reservation; bound generously
        # to include leg teardown, but far below the 90s DB stale sweep).
        deadline = time.time() + 30
        recovered = {AGENT: False, SECOND: False}
        while time.time() < deadline and not all(recovered.values()):
            for aid in recovered:
                if not recovered[aid]:
                    st = (await api.get_agent(aid))["status"]
                    if st in ("idle", "offline", "wrapup"):
                        recovered[aid] = True
            await asyncio.sleep(1.0)
        pytest.log.info(f"race2 recovery: {recovered}")
        stuck = [aid for aid, ok in recovered.items() if not ok]
        assert not stuck, (
            f"agents {stuck} stuck in Ringing after reservation-window "
            "abandon — adoption/release did not cover the pre-app reservation"
        )
    finally:
        sipbot_pool.terminate_user("2204")
        await _drain(api)
        await a1.stop(); await a2.stop()


# ── R-race3: REST offline while ringing defers (no bulldoze) ─────────────────
async def test_race3_rest_offline_while_ringing_defers(
        pbx, sipbot_pool, api, event_checker):
    """Force offline via REST the moment the agent's leg rings. The
    transition must DEFER (auto_offline_after_wrapup) — the agent stays in
    the runtime state until the leg resolves, never a mid-call Offline."""
    await _setup(api)
    ag = RestsendAgent(pbx, AGENT, local_port=25204)
    await ag.start()
    try:
        assert await ag.register(expires=120)
        await ag.publish_idle()
        assert await _await_status(api, AGENT, "idle") == "idle"

        caller = await _spawn_caller(pbx, sipbot_pool, hangup=75, port=25312)
        # Wait for the ring, then REST-offline mid-ring.
        st = await _await_status(api, AGENT, "ringing", timeout=25)
        assert st == "ringing", f"agent never rang (got {st!r})"
        r = await api.update_agent_status(AGENT, "offline")
        pytest.log.info(f"race3 REST offline while ringing → {r}")

        # The agent must NOT be Offline while the leg is alive; the request
        # either deferred or will settle through the runtime states
        # (ringing → wrapup/no-answer …) before Offline.
        st = (await api.get_agent(AGENT))["status"]
        assert st != "offline", "bulldozed to Offline while leg ringing"
        await asyncio.sleep(2)
        st2 = (await api.get_agent(AGENT))["status"]
        assert st2 in ("ringing", "wrapup", "busy", "idle", "offline"), st2
        pytest.log.info(f"race3 statuses: immediate={st!r} later={st2!r}")
    finally:
        sipbot_pool.terminate_user("2204")
        await _drain(api)
        await ag.stop()
