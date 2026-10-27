"""ACD cluster e2e — 整理期间不会被调度 (wrapup / ACW gating, dual node).

Scenario 2 from the 2026-10-15 prd acceptance request:

  After a call ends the agent enters 话后处理 (wrapup) — a non-schedulable
  state. A call that is queued at that moment MUST wait: the wrapup agent is
  never dispatched, the call stays counted as waiting on BOTH nodes, and only
  once the agent is back to `idle` (manual ACW end / timer) does the call
  dispatch. Exactly one INVITE per call — no double-dial while wrapping up.
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


GROUP = "e2e-wu1"
ROUTE_BASE = "7411"  # caller dials 74111


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


def monitor(prd: PrdCluster) -> dict[str, dict]:
    return {n.name: n.get("/cc/monitor")["data"] for n in prd.nodes}


def summary(snap: dict, key: str) -> int:
    return int((snap.get("summary") or {}).get(key) or 0)


async def test_wrapup_agent_not_dispatched(prd, fleet, pool, topology_factory):
    topo = topology_factory(
        tag="wu1",
        groups=[{"id": GROUP, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": "1002", "groups": [GROUP], "max_concurrency": 1}],
        route_base=ROUTE_BASE,
    )
    prd.drain(["1002"])
    (agent,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    base = agent.invite_count()

    # call A lands on the only agent and is answered → agent busy
    call_a = await place_call(prd, pool, topo)
    assert await agent.answer_next(timeout=30), "call A should connect"
    prd.assert_agent_status_both_nodes("1002", "busy", timeout=15)
    call_a_id = prd.call_node.only_active_call_id(ROUTE_BASE + "1")

    # call B enters while the agent is busy → must WAIT
    call_b = await place_call(prd, pool, topo)
    prd.wait_for(
        lambda: all(summary(s, "waiting") >= 1 for s in monitor(prd).values()),
        timeout=15, what="call B waiting on both nodes while agent busy",
    )
    assert agent.invite_count() - base == 1, "busy agent must not receive call B yet"

    # end ONLY call A → agent goes to 整理 (wrapup) on both nodes
    prd.call_node.end_call(call_a_id)
    await agent.reset()  # clear the client's stuck in-call state after server BYE
    prd.wait_for(
        lambda: all(n.agent_status("1002") == "wrapup" for n in prd.nodes),
        timeout=25, what="agent 1002 in wrapup on both nodes",
    )

    # ── THE invariant: while wrapping up, call B must NOT ring anyone ──────
    moved = False
    deadline = asyncio.get_event_loop().time() + 8
    while asyncio.get_event_loop().time() < deadline:
        statuses = {n.name: n.agent_status("1002") for n in prd.nodes}
        invites = agent.invite_count() - base
        assert invites == 1, (
            f"wrapup agent was dispatched (invites={invites}); "
            f"statuses={statuses} — 整理期间不应被调度"
        )
        assert all(summary(s, "waiting") >= 1 for s in monitor(prd).values()), (
            "call B must stay counted as waiting while the agent wraps up"
        )
        if all(v != "wrapup" for v in statuses.values()):
            moved = True
            break
        await asyncio.sleep(1)

    # ── end ACW → idle → call B dispatches, exactly one INVITE ────────────
    for node in prd.nodes:
        with suppress(PrdError):
            node.end_wrapup("1002")
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=20)

    await agent.wait_ring(timeout=30)
    assert await agent.answer_pending(), "call B should connect once the agent is idle"
    assert agent.invite_count() - base == 2, (
        f"expected exactly 2 INVITEs (one per call), got {agent.invite_count() - base}"
    )
    prd.assert_agent_status_both_nodes("1002", "busy", timeout=15)

    prd.call_node.end_all_active_calls()
    prd.drain(["1002"])
