"""ACD cluster e2e — 默认 ACD (LongestIdle) 空闲优先调度 (dual node).

Scenario 3 from the 2026-10-15 prd acceptance request:

  With the default ACD policy (``longest_idle``) the call must go to the agent
  that has been idle the LONGEST, across the merged cluster view — not to
  whichever node happened to enumerate it first. Two dispatches prove the
  ordering rotates with real idle time: agent A (longer idle) wins the first
  call, and once A has handled it and gone idle again, B (now longer idle)
  wins the next.
"""

from __future__ import annotations

import asyncio
import os
from contextlib import suppress

import pytest

from helpers.prd_cluster import PrdCluster, PrdError  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd, pytest.mark.slow]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)


GROUP = "e2e-li1"
ROUTE_BASE = "7421"  # caller dials 74211
IDLE_GAP_SECS = 10  # make agent A unambiguously longer-idle than B


async def place_call(prd: PrdCluster, pool, topo, *, hangup: int = 180):
    caller = pool.caller(
        target=topo.target_uri(GROUP, entry="call"),
        username="1004", password="123456", hangup=hangup,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, (
        f"caller did not get 200 OK entering queue {GROUP}\n"
        f"{str(caller.output)[-1200:]}"
    )
    return caller


async def _dispatch_winner(a1, a2, base1: int, base2: int, timeout: float = 30):
    """First agent to receive a NEW invite (by invite-count delta).

    Uses the authoritative INVITE delta instead of the UA cursor: fleet UAs are
    long-lived across tests, so a stale unconsumed ring could otherwise satisfy
    a cursor-based `wait_ring` without any real dispatch.
    """
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        d1 = a1.invite_count() - base1
        d2 = a2.invite_count() - base2
        if d1 >= 1 or d2 >= 1:
            return (a1, "1001") if d1 >= d2 else (a2, "1002")
        await asyncio.sleep(0.2)
    pytest.fail(
        f"neither agent was dispatched within {timeout}s "
        f"(deltas: 1001={a1.invite_count() - base1}, "
        f"1002={a2.invite_count() - base2})"
    )


async def test_default_longest_idle_selection(prd, fleet, pool, topology_factory):
    topo = topology_factory(
        tag="li1",
        groups=[{"id": GROUP, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[
            {"id": "1001", "groups": [GROUP], "max_concurrency": 1},
            {"id": "1002", "groups": [GROUP], "max_concurrency": 1},
        ],
        route_base=ROUTE_BASE,
    )
    prd.drain(["1001", "1002"])

    # Register A first, let it age, then B → A is the longest-idle candidate.
    (a1,) = await fleet.agents(["1001"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    await asyncio.sleep(IDLE_GAP_SECS)
    (a2,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)

    base1, base2 = a1.invite_count(), a2.invite_count()

    # ── dispatch 1: longest-idle (A) wins ────────────────────────────────
    await place_call(prd, pool, topo)
    winner, wid = await _dispatch_winner(a1, a2, base1, base2)
    assert wid == "1001", (
        f"default ACD must pick the longest-idle agent 1001, got {wid} "
        f"(invites: 1001={a1.invite_count() - base1}, "
        f"1002={a2.invite_count() - base2})"
    )
    assert await winner.answer_pending(), "longest-idle agent should connect"
    prd.assert_agent_status_both_nodes("1001", "busy", timeout=15)

    # end call 1 → A wraps up → force idle; B has now been idle longer
    prd.call_node.end_all_active_calls()
    await a1.reset()
    for node in prd.nodes:
        with suppress(PrdError):
            node.end_wrapup("1001")
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=25)

    # ── dispatch 2: B (now longer idle) wins ─────────────────────────────
    base1, base2 = a1.invite_count(), a2.invite_count()
    await place_call(prd, pool, topo)
    winner2, wid2 = await _dispatch_winner(a1, a2, base1, base2)
    assert wid2 == "1002", (
        f"after 1001 handled a call, the now-longer-idle 1002 must win, got {wid2} "
        f"(invites: 1001={a1.invite_count() - base1}, "
        f"1002={a2.invite_count() - base2})"
    )
    assert await winner2.answer_pending(), "second longest-idle agent should connect"

    prd.call_node.end_all_active_calls()
    prd.drain(["1001", "1002"])
