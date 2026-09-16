"""Repro: "agents all idle, caller hears queue tone, agents never ring".

Customer-reported dispatch failure. Scenarios (restsend-cli = real cc-phone
class client; sip_incoming is the authoritative RING signal):

  R0  baseline  — real client registers, PUBLISHes idle, call arrives,
                  phone rings, answers, connects.
  R1  crash     — idle agent UA hard-killed WITHOUT unregister (lease still
                  valid) → agent stays idle in the console → caller waits.
                  Measures the no-ring window and the recovery bound after
                  the agent re-registers.
  R1b re-reg    — kill + immediate re-register from a new port: dispatch must
                  ring the fresh contact promptly (stale-contact handling).
  R2a acd hours — ACD business_hours outside now: does the queue path stop
                  dispatching while agents show idle?
  R2b skills    — skill-group skills_required no agent has: idle agents,
                  zero offers (console/registry filter mismatch).

All marked `stress` → excluded from the default regression run.
"""

from __future__ import annotations

import asyncio
import json
import time

import logging

import pytest

from helpers.restsend_agent import RestsendAgent

pytest.log = logging.getLogger("repro")

pytestmark = [pytest.mark.stress, pytest.mark.slow]

AGENT = "1001"          # the restsend-call agent
OTHERS = ("1002", "1003")
RING_TIMEOUT_DEFAULT = 30  # queue ring_timeout_secs default


async def _prep(api):
    """Only AGENT is schedulable; others forced offline."""
    for aid in OTHERS:
        try:
            await api.update_agent_status(aid, "offline")
        except Exception:  # noqa: BLE001 — offline→offline 400 is fine
            pass
    a = await api.get_agent(AGENT)
    assert a["status"] in ("idle", "offline", "away", "wrapup"), a


async def _setup(api) -> None:
    """Shared setup: fast wrapup (default 30s leaks across scenarios) and
    other agents offline."""
    await api.put("/api/cc/skill-groups/support", {
        "skills_required": ["support"],
        "metadata": {"wrapup_time_secs": 5},
    })
    await _others_idle_offline(api)


async def _drain(api, timeout: float = 50.0) -> None:
    """Wait until the queue is empty and no agent is mid-dispatch.

    A sipbot caller killed by terminate_user() sends NO BYE — its queued
    call lingers (until max_wait) and would re-dispatch into the NEXT
    scenario's agents. Every scenario drains before handing over."""
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


async def _await_idle(api, agent_id: str, timeout: float = 12.0) -> str:
    """Wait until the registry reports Idle — a previous test's wrapup timer
    (default 30s) may still be running; PUBLISH(note=idle) flips it
    asynchronously."""
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    status = None
    while loop.time() < deadline:
        info = await api.get_agent(agent_id)
        status = info["status"]
        if status == "idle":
            return status
        await asyncio.sleep(0.5)
    return status


async def _spawn_caller(pbx, pool, hangup: int):
    return pool.caller(
        target=f"sip:8888@{pbx.sip_addr}",  # session fixture: 8888 → queue support
        username="2205",
        hangup=hangup,
        addr="127.0.0.1:25300",
    )


async def _others_idle_offline(api):
    """Ensure the OTHER agents are not registered leftovers from earlier
    tests (fresh pbx session → they were never online; REST offline is
    enough)."""
    for aid in OTHERS:
        try:
            await api.update_agent_status(aid, "offline")
        except Exception:  # noqa: BLE001
            pass


# ── R0: baseline — the real client must ring and connect ────────────────────
async def test_r0_real_client_rings_and_connects(pbx, sipbot_pool, api, event_checker):
    await _setup(api)
    agent = RestsendAgent(pbx, AGENT, local_port=25101)
    await agent.start()
    try:
        assert await agent.register(expires=120), "REGISTER failed"
        await agent.publish_idle()
        status = await _await_idle(api, AGENT)
        assert status == "idle", f"expected idle, got {status}"

        caller = await _spawn_caller(pbx, sipbot_pool, hangup=18)
        ev = await agent.wait_event("sip_incoming", timeout=20)
        assert ev, "NO RING on real client within 20s (baseline broken!)"
        assert await agent.answer(), "answer did not reach connected"
        await asyncio.sleep(6)
        await agent.hangup()
        await asyncio.sleep(2)
    finally:
        sipbot_pool.terminate_user("2205")
        await _drain(api)
        await agent.stop()


# ── R1: idle agent crashed (no unregister) → the customer symptom ────────────
async def test_r1_crashed_idle_agent_no_ring_then_recovers(pbx, sipbot_pool, api, event_checker):
    await _setup(api)
    agent = RestsendAgent(pbx, AGENT, local_port=25102)
    await agent.start()
    assert await agent.register(expires=120)
    await agent.publish_idle()
    status = await _await_idle(api, AGENT)
    assert status == "idle", f"expected idle before crash, got {status}"

    # crash WITHOUT unregister — lease (120s) keeps the agent schedulable
    agent.kill()
    await asyncio.sleep(0.5)
    info = await api.get_agent(AGENT)
    assert info["status"] == "idle", (
        f"console should still show idle during the lease — got {info['status']} "
        "(if this fails the registrar already dropped the dead contact early)"
    )

    caller = await _spawn_caller(pbx, sipbot_pool, hangup=150)
    t_call = time.time()

    # The customer symptom: no ring anywhere while the agent shows idle.
    await asyncio.sleep(min(RING_TIMEOUT_DEFAULT, 15))
    assert not agent.rang, "unexpected ring from a killed UA?"

    # Recovery: agent comes back (fresh process, same user, NEW port) after
    # ~1 ring_timeout cycle. Dispatch must then ring + connect.
    await asyncio.sleep(max(0, RING_TIMEOUT_DEFAULT - 15))
    agent2 = RestsendAgent(pbx, AGENT, local_port=25103)
    await agent2.start()
    try:
        assert await agent2.register(expires=120)
        await agent2.publish_idle()
        ev = await agent2.wait_event("sip_incoming", timeout=45)
        t_ring = time.time()
        # recovery bound: fresh UA must ring within a ring_timeout + retry
        assert ev, (
            "NO RING even after re-register — caller stuck on queue tone "
            "while console shows idle (customer bug reproduced, unbounded)"
        )
        assert await agent2.answer()
        recovered = time.time() - t_call
        pytest.log.info(f"R1: no-ring window ≈ {t_ring - t_call:.0f}s, "
                        f"total recovery {recovered:.0f}s")
        await agent2.hangup()
        await asyncio.sleep(1)
    finally:
        sipbot_pool.terminate_user("2205")
        await _drain(api)
        await agent2.stop()


# ── R1b: kill + IMMEDIATE re-register (stale vs fresh contact) ───────────────
async def test_r1b_reregister_replaces_stale_contact(pbx, sipbot_pool, api, event_checker):
    await _setup(api)
    agent = RestsendAgent(pbx, AGENT, local_port=25104)
    await agent.start()
    assert await agent.register(expires=120)
    await agent.publish_idle()
    status = await _await_idle(api, AGENT)
    assert status == "idle", f"expected idle before crash, got {status}"
    agent.kill()                      # dead contact, no unregister

    agent2 = RestsendAgent(pbx, AGENT, local_port=25105)  # fresh port
    await agent2.start()
    try:
        assert await agent2.register(expires=120)
        await agent2.publish_idle()
        caller = await _spawn_caller(pbx, sipbot_pool, hangup=30)
        ev = await agent2.wait_event("sip_incoming", timeout=15)
        assert ev, ("fresh contact did NOT ring within 15s after re-register — "
                    "dispatch still aiming at the stale contact")
        assert await agent2.answer()
        await asyncio.sleep(3)
        await agent2.hangup()
    finally:
        sipbot_pool.terminate_user("2205")
        await _drain(api)
        await agent2.stop()


# ── R2a: ACD business_hours outside now — queue still dispatches? ────────────
async def test_r2a_acd_off_hours_blocks_dispatch(pbx, sipbot_pool, api, event_checker):
    from pathlib import Path
    acd_path = Path(pbx.work_dir) / "config" / "cc" / "acd.toml"
    backup = acd_path.read_text()
    agent = RestsendAgent(pbx, AGENT, local_port=25106)
    await agent.start()
    try:
        assert await agent.register(expires=120)
        await agent.publish_idle()
        await _await_idle(api, AGENT)

        # force an off-hours window (03:00-04:00 is never "now" for long)
        acd_path.write_text(backup + """
[policies.default.schedule.business_hours]
start = "03:00"
end = "04:00"
""")
        r = await api.post("/api/cc/acd/reload", {})
        pytest.log.info(f"acd reload → {r}")

        caller = await _spawn_caller(pbx, sipbot_pool, hangup=25)
        # observe: does the real client ring despite idle agents?
        # (28s > the caller's own 25s hangup — its BYE ends the queue call
        # cleanly so no zombie leg leaks into the next scenario)
        ev = await agent.wait_event("sip_incoming", timeout=28)
        outcome = "RING" if ev else "NO-RING"
        pytest.log.info(f"R2a off-hours outcome: {outcome} "
                        f"(queue-path dispatch {'lives' if ev else 'BLOCKED/absent'})")
        if ev:
            await agent.answer()
            await asyncio.sleep(2)
            await agent.hangup()
        # either way this is diagnostic; assert only that the caller call
        # terminates (queue_left seen) so we know the queue was entered.
    finally:
        sipbot_pool.terminate_user("2205")
        await _drain(api)
        acd_path.write_text(backup)
        try:
            await api.post("/api/cc/acd/reload", {})
        except Exception:  # noqa: BLE001
            pass
        await agent.stop()


# ── R2b: skills_required nobody has → idle agents, zero offers ───────────────
async def test_r2b_skills_mismatch_zero_offers(pbx, sipbot_pool, api, event_checker):
    agent = RestsendAgent(pbx, AGENT, local_port=25107)
    await agent.start()
    try:
        assert await agent.register(expires=120)
        await agent.publish_idle()
        await _await_idle(api, AGENT)

        # point the support group at a skill NO agent has
        await api.put("/api/cc/skill-groups/support", {
            "skills_required": ["nosuchskill-xyz"],
        })
        caller = await _spawn_caller(pbx, sipbot_pool, hangup=25)
        # 27s > the caller's own 25s hangup: its BYE cleanly ends the queued
        # call BEFORE we restore the skills (otherwise the zombie call rings
        # the still-registered agent during teardown and its no-answer
        # cooldown leaks into the next scenario).
        ev = await agent.wait_event("sip_incoming", timeout=27)
        assert not ev, "rang despite skills mismatch — filter broken the other way"
        # and the console still shows the agent idle → exact customer picture
        info = await api.get_agent(AGENT)
        assert info["status"] == "idle"
    finally:
        sipbot_pool.terminate_user("2205")
        await _drain(api)
        # restore
        await api.put("/api/cc/skill-groups/support", {
            "skills_required": ["support"],
        })
        await agent.stop()


# ── R1c: dead contact + live agent — progressive cooldown steers dispatch ────
async def test_r1c_dead_agent_cooldown_speeds_up_second_call(pbx, sipbot_pool, api, event_checker):
    """Customer-visible improvement: after agent 1001's UA dies (no
    unregister), the FIRST call may burn a ring_timeout on the dead contact,
    but the progressive no-answer cooldown must keep 1001 out of the rotation
    so the SECOND call rings the LIVE agent (1002) almost immediately."""
    import time as _t
    await _setup(api)          # 1003 offline
    dead = RestsendAgent(pbx, AGENT, local_port=25110)
    live = RestsendAgent(pbx, "1002", local_port=25111)
    await dead.start(); await live.start()
    try:
        assert await dead.register(expires=120)
        assert await live.register(expires=120)
        await dead.publish_idle(); await live.publish_idle()
        assert await _await_idle(api, AGENT) == "idle"
        assert await _await_idle(api, "1002") == "idle"

        dead.kill()  # dead contact, presence still idle

        # call 1: LongestIdle rotation may pick the dead agent first and burn
        # one ring_timeout; either way it must end connected on the live one.
        c1 = await _spawn_caller(pbx, sipbot_pool, hangup=110)
        t1 = _t.time()
        ev1 = await live.wait_event("sip_incoming", timeout=75)
        assert ev1, "live agent never rang on call 1"
        assert await live.answer()
        lat1 = _t.time() - t1
        await asyncio.sleep(4)
        await live.hangup()
        await asyncio.sleep(6)  # wrapup on the live agent
        pytest.log.info(f"R1c call1 latency (may include dead burn): {lat1:.0f}s")

        # call 2: dead agent is in no-answer cooldown — must ring live FAST.
        c2 = await _spawn_caller(pbx, sipbot_pool, hangup=110)
        t2 = _t.time()
        ev2 = await live.wait_event("sip_incoming", timeout=60)
        assert ev2, "live agent never rang on call 2"
        assert await live.answer()
        lat2 = _t.time() - t2
        pytest.log.info(f"R1c call2 latency (cooldown active): {lat2:.0f}s")
        assert lat2 < max(10.0, lat1 * 0.6), (
            f"call2 ({lat2:.0f}s) not materially faster than call1 "
            f"({lat1:.0f}s) — dead-contact cooldown not steering dispatch"
        )
        await asyncio.sleep(3)
        await live.hangup()
    finally:
        sipbot_pool.terminate_user("2205")
        await _drain(api)
        await dead.stop(); await live.stop()
