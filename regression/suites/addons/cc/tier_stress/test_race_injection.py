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
    """Answer lands just AFTER the 30s ring timeout flipped the agent into
    no-answer Wrapup. The bridge must still stand and the agent must be Busy
    (not Wrapup) with current_calls=1 — otherwise the cooldown timer flips
    them Idle mid-call and the ACD double-dispatches."""
    await _setup(api)
    late = RestsendAgent(pbx, AGENT, local_port=25201)
    await late.start()
    try:
        assert await late.register(expires=120)
        await late.publish_idle()
        assert await _await_status(api, AGENT, "idle") == "idle"

        caller = await _spawn_caller(pbx, sipbot_pool, hangup=110, port=25310)
        t0 = time.time()

        # Do NOT answer when it rings; wait until the registry shows the
        # no-answer Wrapup (ring timeout ~30s), then answer immediately —
        # the 200 OK lands right after the timeout's transition.
        status = await _await_status(api, AGENT, "wrapup", timeout=45)
        assert status == "wrapup", f"expected wrapup after ring timeout, got {status}"
        await late.cmd({"cmd": "sip_answer"})

        ev = await late.wait_event(
            "state_changed",
            predicate=lambda e: e.get("name") == "connected",
            timeout=15,
        )
        assert ev, "late answer did not connect the call"
        pytest.log.info(f"race1: connected at t+{time.time() - t0:.0f}s")

        # THE assertion: agent must now be Busy on this call, not stuck in
        # wrapup, and capacity consumed.
        st = await _await_status(api, AGENT, "busy", timeout=8)
        assert st == "busy", (
            f"late answer left agent in {st!r} — cooldown timer will flip "
            "them Idle mid-call (double-dispatch window)"
        )
        info = await api.get_agent(AGENT)
        assert info["current_calls"] == 1, info

        await asyncio.sleep(4)
        await late.hangup()
        await asyncio.sleep(2)
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
        await a1.publish_idle(); await a2.publish_idle()
        assert await _await_status(api, AGENT, "idle") == "idle"
        assert await _await_status(api, SECOND, "idle") == "idle"

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
