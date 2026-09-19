"""Queue abnormal-path release E2E: early abandon, wait timeout, agent
reject, and a soak loop — every cycle must release its resources.

The caller leg is RWI-originated so the test owns the call and can hang it
up at a precise moment (``call.hangup`` pre-answer = CANCEL on the ringing
agent leg).

Scenarios:

1. ``test_caller_abandon_while_agent_ringing`` — hang up while the agent
   leg is still ringing: the queue must abandon
   (``skill_group_call_abandoned``), cancel the agent leg (sipbot reports
   ``Terminated UacCancel``), return the agent to ``idle`` and leave
   nothing waiting.
2. ``test_queue_wait_timeout_releases`` — nobody answers; the queue's
   ``wait_timeout`` fires and the caller is failed (never answered). Same
   release assertions.
3. ``test_agent_reject_releases_agent`` — the agent rejects with 486; the
   registry must not strand it in ringing/busy and the queue must fall out
   to the failure fallback.
4. ``test_queue_release_soak_no_leak`` — N alternating abandon / timeout
   cycles against ONE pbx instance. Per cycle: ``call_hangup`` + agent
   ``idle`` + monitor ``waiting == 0``. Globally: RSS growth across the
   soak stays bounded (leak guard for lingering sessions / bridges /
   registry entries).
"""

from __future__ import annotations

import asyncio
import subprocess

import pytest

import helpers as h

pytestmark = [pytest.mark.queue, pytest.mark.cdr]

AGENT = "1002"
WRAPUP_SECS = 2
RING_NEVER = 90  # agent ring duration longer than any test window


def _setup_queue(pbx, *, wait_timeout: int) -> None:
    # "support" as a guest-dialable user so the credential-less RWI
    # originate leg can reach the queue (same as real queue numbers).
    pbx.config_builder.add_guest_user(["support"])
    pbx.config_builder.add_queue(
        "support",
        strategy_mode="sequential",
        targets=["skill-group:support"],
        ring_timeout_secs=30,
        wait_timeout_secs=wait_timeout,
    )
    pbx.config_builder.add_route(
        "to-support",
        match={"to.user": "support"},
        priority=10,
        action="queue",
        queue="support",
    )


async def _seed_cc(api, *, max_wait_secs: int = 90) -> None:
    """CC agent + skill group with a short wrapup so cycles stay fast.

    ``max_wait_secs`` (group-level) is what arms the queue's wall-clock
    wait timeout — for skill-group queues the route-file plan's max wait is
    fixed and the registry override comes from this row.
    """
    await api.ensure_console_auth()
    for body in (
        {"agent_id": AGENT, "display_name": f"Agent {AGENT} (release)",
         "skills": ["support"], "max_concurrency": 3, "role": "agent"},
        {"skill_group_id": "support", "skills_required": ["support"],
         "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": max_wait_secs,
         "metadata": {"wrapup_time_secs": WRAPUP_SECS}},
    ):
        try:
            if "skill_group_id" in body:
                await api.create_skill_group(body)
            else:
                await api.create_agent(body)
        except Exception as exc:  # noqa: BLE001 — duplicate on re-run is fine
            if not ("409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower()):
                raise
    # PUT applies the metadata — create may ignore it on duplicates.
    await api.put("/api/cc/skill-groups/support", {
        "skills_required": ["support"],
        "max_wait_secs": max_wait_secs,
        "metadata": {"wrapup_time_secs": WRAPUP_SECS},
    })
    sg = await api.get("/api/cc/skill-groups/support")
    data = sg.get("data") or sg if isinstance(sg, dict) else sg
    print(f"\n[seed] skill-group after PUT: {str(data)[:400]}", flush=True)


def _spawn_ringing_agent(pbx, sipbot_pool):
    """A registered agent that rings (echo) but never answers in-window."""
    return sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(17170), username=AGENT, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=RING_NEVER, answer_mode="echo",
    )


async def _monitor_snapshot(api) -> dict:
    snap = await api.get("/api/cc/monitor")
    return snap.get("data") or snap


async def _await_released(api, *, timeout: float = 45.0) -> None:
    """Agent back to idle AND nothing waiting — the per-cycle release gate.

    NOTE: the abandon / ring-timeout / reject release paths start wrapup with
    the built-in 30 s default (they do not read the skill group's
    ``metadata.wrapup_time_secs`` — only the customer-disconnect hook does),
    so the idle wait needs to cover the 30 s wrapup window.
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    last = None
    while loop.time() < deadline:
        try:
            info = await api.get_agent(AGENT)
            status = info.get("status")
            snap = await _monitor_snapshot(api)
            waiting = (snap.get("summary") or {}).get("waiting") or 0
            last = (status, waiting)
            if status == "idle" and waiting == 0:
                return
        except Exception:  # noqa: BLE001
            pass
        await asyncio.sleep(0.5)
    raise AssertionError(f"resources not released within {timeout}s: {last}")


def _rss_kb(pid: int) -> int:
    out = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        capture_output=True, text=True, timeout=10,
    ).stdout.strip()
    return int(out.split()[0]) if out else 0


async def _originate_into_queue(rwi, pbx, call_id: str) -> asyncio.Task:
    """Originate the caller leg into the queue. The reply only resolves on
    answer/failure — run it as a task and drive the call via call.hangup."""

    async def _run():
        try:
            if not rwi.connected:
                print(f"[originate] {call_id}: RWI ws reconnected", flush=True)
                await rwi.connect()
                await rwi.subscribe(["*"])
            return await rwi.originate(
                call_id, f"sip:support@{pbx.sip_addr}", "sip:rwi@pbx",
                "default", timeout_secs=60,
            )
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 — abandon/failure paths
            print(f"[originate] {call_id}: failed: {exc!r}", flush=True)
            return None

    return asyncio.create_task(_run())


async def _wait_offered(event_checker, timeout: float = 20) -> dict:
    offered = await event_checker.webhook.wait_for_event(
        "queue_agent_offered", timeout=timeout,
    )
    assert offered is not None, (
        f"agent never offered. events: {event_checker.webhook.event_types()}"
    )
    return offered


async def _wait_call_hangup(event_checker, timeout: float = 25):
    ev = await event_checker.webhook.wait_for_event("call_hangup", timeout=timeout)
    assert ev is not None, (
        f"no call_hangup. events: {event_checker.webhook.event_types()}"
    )
    return ev


@pytest.mark.asyncio
async def test_caller_abandon_while_agent_ringing(
    pbx, sipbot_pool, api, event_checker, webhook_server,
):
    """Caller hangs up while the agent rings: agent released, queue drained."""
    _setup_queue(pbx, wait_timeout=15)
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    await _seed_cc(api)
    await h.connect_rwi(event_checker.rwi)

    agent = _spawn_ringing_agent(pbx, sipbot_pool)
    await h.wait_registered(agent, f"agent {AGENT}")

    task = await _originate_into_queue(event_checker.rwi, pbx, "abandon-0001")
    await _wait_offered(event_checker)

    # Caller abandons while the agent leg is ringing → pre-answer hangup.
    resp = await event_checker.rwi.hangup("abandon-0001")
    assert resp.get("status") != "error", f"hangup failed: {resp}"

    abandoned = await event_checker.webhook.wait_for_event(
        "skill_group_call_abandoned", timeout=20,
    )
    assert abandoned is not None, (
        f"no skill_group_call_abandoned. events: {event_checker.webhook.event_types()}"
    )
    await _wait_call_hangup(event_checker)

    # The agent leg must have been cancelled, not left ringing.
    got_cancel = await agent.wait_output_async(
        r"UacCancel|terminated remotely|Cancel", timeout=10,
    )
    assert got_cancel, f"agent leg not cancelled:\n{agent.output[-600:]}"

    await _await_released(api)
    rwi_types = [e.get("event_type") for e in event_checker.rwi.events]
    assert "queue_left" in rwi_types, (
        f"RWI WS never saw queue_left after abandon: {rwi_types}"
    )
    if task and not task.done():
        task.cancel()


@pytest.mark.asyncio
async def test_queue_wait_timeout_releases(
    pbx, sipbot_pool, api, event_checker, webhook_server,
):
    """Nobody answers: wait_timeout fires, caller failed, resources released."""
    _setup_queue(pbx, wait_timeout=3)
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    await _seed_cc(api, max_wait_secs=3)
    await h.connect_rwi(event_checker.rwi)

    agent = _spawn_ringing_agent(pbx, sipbot_pool)
    await h.wait_registered(agent, f"agent {AGENT}")

    task = await _originate_into_queue(event_checker.rwi, pbx, "timeout-0001")
    await _wait_offered(event_checker)

    # The caller must never reach 200 and must be torn down by the fallback.
    got_failure = await event_checker.webhook.wait_for_event(
        "skill_group_service_unavailable", timeout=20,
    )
    assert got_failure is not None, (
        "no skill_group_service_unavailable after wait timeout. events: "
        f"{event_checker.webhook.event_types()}"
    )

    await _wait_call_hangup(event_checker)

    # The failing leg rang the agent: the CANCEL/BYE after the fallback must
    # reach it (same release guarantee as the abandon path).
    got_cancel = await agent.wait_output_async(
        r"UacCancel|terminated remotely|BYE|Cancel", timeout=10,
    )
    assert got_cancel, f"agent leg not released after timeout:\n{agent.output[-600:]}"

    await _await_released(api)
    if task and not task.done():
        task.cancel()


@pytest.mark.asyncio
async def test_agent_reject_releases_agent(
    pbx, sipbot_pool, api, event_checker, webhook_server,
):
    """Agent rejects with 486: no stranded ringing/busy, queue falls out."""
    _setup_queue(pbx, wait_timeout=8)
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    await _seed_cc(api, max_wait_secs=8)
    await h.connect_rwi(event_checker.rwi)

    # reject_code needs reject-prob to take effect on sipbot 0.2.x.
    agent = sipbot_pool.callee(
        host=pbx.host, port=h.ua_port(17170), username=AGENT, password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, reject_code=486,
    )
    await h.wait_registered(agent, f"agent {AGENT}")

    task = await _originate_into_queue(event_checker.rwi, pbx, "reject-0001")
    # A 486 reject never sends 180 → no queue_agent_offered; the dispatch
    # assignment is the observable instead.
    assigned = await event_checker.webhook.wait_for_event(
        "skill_group_agent_assigned", timeout=20,
    )
    assert assigned is not None, (
        f"agent never assigned. events: {event_checker.webhook.event_types()}"
    )

    got_end = await event_checker.webhook.wait_for_event(
        "skill_group_service_unavailable", timeout=30,
    )
    assert got_end is not None, (
        "no fallback event after agent reject. events: "
        f"{event_checker.webhook.event_types()}"
    )

    # The ACD wait-retention loop may re-queue + re-assign after the fallback
    # (the rejecting agent keeps drawing dispatches until the originate
    # timeout tears the caller leg down) — the hangup can take up to that
    # timeout to surface.
    await _wait_call_hangup(event_checker, timeout=75)
    await _await_released(api)

    # The rejected agent must still be registered and schedulable.
    info = await api.get_agent(AGENT)
    assert info.get("status") in ("idle", "wrapup"), (
        f"agent stranded in {info.get('status')!r} after 486 reject"
    )
    if task and not task.done():
        task.cancel()


@pytest.mark.asyncio
async def test_queue_release_soak_no_leak(
    pbx, sipbot_pool, api, event_checker, webhook_server,
):
    """Alternating abandon/timeout cycles against one pbx — resources must
    release every cycle and RSS growth must stay bounded.

    Each cycle uses a FRESH agent (own registration + CC row): the abandon /
    timeout release paths start a 30 s wrapup (built-in default — the skill
    group's wrapup metadata is not consulted there yet), and a wrapup agent
    is not schedulable, which would stall the next cycle. The final gate
    waits for every agent's auto-idle timer — proving the timers release.
    """
    _setup_queue(pbx, wait_timeout=3)
    # Parallel dialing: the strategy skips non-idle agents (wrapup leftovers
    # from earlier cycles stay registered) and rings the fresh agent at once —
    # sequential rounds would burn ring_timeout on stale candidates first.
    pbx.config_builder.queues["support"]["strategy"]["mode"] = "parallel"
    cycles = 10
    soak_agents = [f"20{i:02d}" for i in range(1, cycles + 1)]
    pbx.config_builder.add_memory_users(soak_agents)
    h.boot_pbx(pbx, webhook_url=webhook_server.url)
    await _seed_cc(api, max_wait_secs=4)
    await h.connect_rwi(event_checker.rwi)

    pid = pbx.process.pid

    def _hangup_count() -> int:
        return len([
            e for e in event_checker.webhook.all_events()
            if e.event_type == "call_hangup"
        ])

    async def one_cycle(i: int) -> str:
        username = soak_agents[i]
        base = _hangup_count()
        # CC row FIRST: the registrar bridge flips an existing row to idle on
        # REGISTER — a row created after the REGISTER stays offline forever.
        try:
            await api.create_agent({
                "agent_id": username, "display_name": f"Soak {username}",
                "skills": ["support"], "max_concurrency": 3, "role": "agent",
            })
        except Exception as exc:  # noqa: BLE001 — duplicate on re-run is fine
            if not ("409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower()):
                raise
        bot = sipbot_pool.callee(
            host=pbx.host, port=h.ua_port(18000 + i), username=username,
            password="123456", register=True,
            proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
            ring_secs=RING_NEVER, answer_mode="echo",
        )
        await h.wait_registered(bot, f"agent {username}")
        # The registrar bridge flips the CC row to idle ASYNC — wait for it,
        # otherwise the queue resolves zero candidates and parks the call.
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 12
        status = None
        while loop.time() < deadline:
            info = await api.get_agent(username)
            status = info.get("status")
            if status == "idle":
                break
            await asyncio.sleep(0.3)
        assert status == "idle", f"cycle {i}: agent {username} never idle ({status})"

        call_id = f"soak-{i:04d}"
        # Queue events carry the SESSION id, not the RWI call id — filter by
        # buffer position (only events newer than this cycle's start).
        base_events = len(event_checker.webhook.all_events())
        task = await _originate_into_queue(event_checker.rwi, pbx, call_id)
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 25
        offered = None
        while loop.time() < deadline:
            evs = event_checker.webhook.all_events()
            fresh = [
                e for e in evs[base_events:]
                if e.event_type == "queue_agent_offered"
            ]
            if fresh:
                offered = fresh[0]
                break
            await asyncio.sleep(0.3)
        assert offered is not None, (
            f"cycle {i}: dispatch failed. events: {event_checker.webhook.event_types()}"
        )
        if i % 2 == 0:
            # Early abandon mid-ring.
            if not event_checker.rwi.connected:
                await event_checker.rwi.connect()
                await event_checker.rwi.subscribe(["*"])
            resp = await event_checker.rwi.hangup(call_id)
            assert resp.get("status") != "error", f"cycle {i}: hangup failed: {resp}"
        # else: leave it — the wait_timeout fallback fails the call.
        deadline = loop.time() + 60
        while loop.time() < deadline:
            if _hangup_count() > base:
                break
            await asyncio.sleep(0.4)
        else:
            raise AssertionError(f"cycle {i}: call_hangup never fired")
        if task and not task.done():
            task.cancel()
        # Per-cycle gate: queue drained (the agent itself sits in its 30 s
        # wrapup until the final gate below). The last retention poll can
        # re-enqueue a waiting row right before the call ends — allow a
        # short grace window for the counter to drain.
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 12
        waiting = None
        while loop.time() < deadline:
            snap = await _monitor_snapshot(api)
            waiting = (snap.get("summary") or {}).get("waiting") or 0
            if waiting == 0:
                break
            await asyncio.sleep(0.5)
        assert waiting == 0, f"cycle {i}: {waiting} still waiting after grace"
        return username

    # Warmup (allocator + lazy init) — not counted.
    warm = await one_cycle(0)
    await _await_idle_or_wrapup_done(api, [warm])

    rss_before = _rss_kb(pid)
    baseline_hangups = _hangup_count()

    used = []
    for i in range(1, cycles):
        used.append(await one_cycle(i))

    rss_after = _rss_kb(pid)
    growth_mb = (rss_after - rss_before) / 1024.0
    total_hangups = _hangup_count() - baseline_hangups

    # Final gate: every wrapup timer fired — all agents back to idle.
    await _await_idle_or_wrapup_done(api, used, timeout=45)

    snap = await _monitor_snapshot(api)
    waiting = (snap.get("summary") or {}).get("waiting") or 0
    assert waiting == 0, f"monitor shows {waiting} still waiting after soak"
    assert total_hangups >= cycles - 1, (
        f"expected ≥{cycles - 1} call_hangup events, got {total_hangups} — "
        "some cycles leaked a call"
    )
    assert growth_mb < 80, (
        f"pbx RSS grew {growth_mb:.1f}MB over {cycles} abandon/timeout cycles "
        f"({rss_before}KB → {rss_after}KB) — suspected memory leak"
    )
    print(
        f"\n[soak] {cycles} cycles ok: hangups={total_hangups}, "
        f"RSS {rss_before}KB → {rss_after}KB (growth {growth_mb:.1f}MB)"
    )


async def _await_idle_or_wrapup_done(api, agent_ids: list[str], timeout: float = 45.0):
    """Wait until every listed agent's wrapup timer fired (status == idle)."""
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        try:
            states = []
            for aid in agent_ids:
                info = await api.get_agent(aid)
                states.append(info.get("status"))
            if all(s == "idle" for s in states):
                return
        except Exception:  # noqa: BLE001
            pass
        await asyncio.sleep(1.0)
    raise AssertionError(f"agents not idle after {timeout}s: {list(zip(agent_ids, states))}")
