"""ACD cluster e2e — 各自排队、调度准确 (per-queue isolation, dual node).

Scenario 6 from the 2026-10-15 prd acceptance request:

  Two independent skill groups/queues, each served by a DISJOINT agent. Callers
  entering concurrently must queue ONLY in their own group, the per-group count
  must be exact on BOTH nodes, and freeing one group's agent must dispatch only
  that group's call — never the other's. No cross-queue FIFO contamination.
"""

from __future__ import annotations

import asyncio
import os

import pytest

from helpers.prd_cluster import PrdCluster, PrdError  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd, pytest.mark.slow]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)


G1 = "e2e-mq1"
G2 = "e2e-mq2"
ROUTE_BASE = "7441"  # g1 -> 74411, g2 -> 74412


async def place_call(prd: PrdCluster, pool, topo, group_id: str, *, hangup: int = 180):
    caller = pool.caller(
        target=topo.target_uri(group_id, entry="call"),
        username="1004", password="123456", hangup=hangup,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, (
        f"caller did not get 200 OK entering queue {group_id}\n"
        f"{str(caller.output)[-1200:]}"
    )
    return caller


def monitor(prd: PrdCluster) -> dict[str, dict]:
    return {n.name: n.get("/cc/monitor")["data"] for n in prd.nodes}


def summary(snap: dict, key: str) -> int:
    return int((snap.get("summary") or {}).get(key) or 0)


def group_waiting(snap: dict, group_id: str) -> int:
    for g in snap.get("skill_groups") or []:
        if g.get("skill_group_id") == group_id:
            return int(g.get("waiting") or 0)
    return 0


def queued_ids(snap: dict, group_id: str) -> set[str]:
    return {
        q.get("call_id")
        for q in (snap.get("queued_calls") or [])
        if q.get("queue_id") == group_id
    }


def unclaimed(prd: PrdCluster, group_id: str) -> int:
    rows = prd.sql(
        "SELECT COUNT(*) FROM cc_acd_queue "
        f"WHERE queue_id='{group_id}' AND claimed_by IS NULL"
    )
    return int(rows[1][0]) if len(rows) > 1 else 0


async def test_per_queue_isolation(prd, fleet, pool, topology_factory):
    topo = topology_factory(
        tag="mq1",
        groups=[
            {"id": G1, "max_wait_secs": 120, "sla_target_secs": 30},
            {"id": G2, "max_wait_secs": 120, "sla_target_secs": 30},
        ],
        agents=[
            {"id": "1001", "groups": [G1], "max_concurrency": 1},
            {"id": "1002", "groups": [G2], "max_concurrency": 1},
        ],
        route_base=ROUTE_BASE,
    )
    prd.drain(["1001", "1002"])
    (a1,) = await fleet.agents(["1001"])
    (a2,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    base1, base2 = a1.invite_count(), a2.invite_count()

    # park both agents so nothing dispatches until we say so
    prd.set_agent_status("1001", "away")
    prd.set_agent_status("1002", "away")
    prd.assert_agent_status_both_nodes("1001", "away", timeout=15)
    prd.assert_agent_status_both_nodes("1002", "away", timeout=15)

    # one caller per queue, concurrently
    await place_call(prd, pool, topo, G1)
    await place_call(prd, pool, topo, G2)

    prd.wait_for(
        lambda: all(
            group_waiting(s, G1) == 1 and group_waiting(s, G2) == 1
            for s in monitor(prd).values()
        ),
        timeout=15,
        what="each group holds exactly its own caller on both nodes",
    )
    for name, snap in monitor(prd).items():
        assert summary(snap, "waiting") == 2, (
            f"{name}: summary.waiting={summary(snap, 'waiting')} expected 2; "
            f"snap={snap.get('summary')}"
        )
        assert summary(snap, "dispatching") == 0
        ids1, ids2 = queued_ids(snap, G1), queued_ids(snap, G2)
        assert len(ids1) == 1 and len(ids2) == 1, (
            f"{name}: queued attribution cross-contaminated: "
            f"{G1}={ids1}, {G2}={ids2}"
        )
        assert ids1.isdisjoint(ids2)
    assert unclaimed(prd, G1) == 1 and unclaimed(prd, G2) == 1, (
        "shared cc_acd_queue must hold exactly one unclaimed row per group"
    )

    # ── free ONLY group 1's agent → only its call dispatches ─────────────
    prd.set_agent_status("1001", "idle")
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=15)
    assert await a1.wait_ring(timeout=30) is not None, "group 1 call did not dispatch"

    prd.wait_for(
        lambda: all(
            group_waiting(s, G1) == 0 and group_waiting(s, G2) == 1
            and summary(s, "dispatching") == 1
            for s in monitor(prd).values()
        ),
        timeout=15,
        what="group 1 dispatching, group 2 still waiting on both nodes",
    )
    assert a2.invite_count() - base2 == 0, "group 2's agent was dispatched group 1's call"
    assert await a1.answer_pending(), "group 1 call should connect"

    # end ONLY group 1's bridged call (group 2's caller must stay queued)
    g1_ids = prd.call_node.active_call_ids(topo.route_for(G1))
    assert len(g1_ids) == 1, f"expected 1 bridged g1 call, got {g1_ids}"
    prd.call_node.end_call(g1_ids[0])
    await a1.reset()

    # ── free group 2's agent → its call now dispatches ───────────────────
    prd.set_agent_status("1002", "idle")
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=15)
    assert await a2.wait_ring(timeout=40) is not None, "group 2 call did not dispatch"
    assert await a2.answer_pending(), "group 2 call should connect"

    prd.wait_for(
        lambda: all(
            group_waiting(s, G1) == 0 and group_waiting(s, G2) == 0
            for s in monitor(prd).values()
        ),
        timeout=15,
        what="both queues drained after each group connected",
    )

    prd.call_node.end_all_active_calls()
    prd.drain(["1001", "1002"])
