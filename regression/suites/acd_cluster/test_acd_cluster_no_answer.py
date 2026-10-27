"""ACD cluster e2e — 久振未接 (ring no-answer) + 排队计数准确 (dual node).

Scenario 1 + 4 from the 2026-10-15 prd acceptance request:

  1. 久振未接 — a dispatched agent that never answers must have its ringing
     leg CANCELled at the ring timeout and the call must NOT be dropped: the
     customer stays in queue and the call is re-dispatched (a fresh INVITE)
     once the agent is available again. No repeat/duplicate INVITE during the
     first ring, and no "dequeue" of a call that was merely missed.

  4. 排队计数准确 — "进入振铃" leaves the waiting pool (`waiting` 1→0,
     `dispatching` 0→1 on BOTH nodes) and the shared `cc_acd_queue` row carries
     a claim; a miss must never spawn a SECOND row for the same call.

Effective ring timeout on the prd cluster: the CC skill-group dispatch takes
its per-agent ring timeout from `[desk.behavior] ring_no_answer_secs`
(default 30s), NOT from the queue plan's `strategy.wait_timeout_secs`. Because
`server_ring_no_answer=false` on prd, the no-answer state is client-driven:
after the 30s ring the leg is cancelled and the claim released, and the agent
is only released back to `idle` after a client-confirmation budget (~32s),
after which the queued call re-dispatches. The observation window therefore
spans ~90s.
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


RING_TIMEOUT_SECS = 30   # informational — prd uses [desk.behavior].ring_no_answer_secs
REQUEUE_OBSERVE_SECS = 90
GROUP = "e2e-na1"
ROUTE_BASE = "7401"  # caller dials 74011


# ── helpers ─────────────────────────────────────────────────────────────────

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


def queue_rows(prd: PrdCluster) -> list[dict]:
    return [r for r in prd.acd_queue_rows() if r["queue_id"] == GROUP]


def force_idle(prd: PrdCluster, agent_id: str) -> None:
    for node in prd.nodes:
        if node.agent_status(agent_id) == "wrapup":
            with suppress(PrdError):
                node.end_wrapup(agent_id)
    with suppress(PrdError):
        prd.set_agent_status(agent_id, "idle")


# ── test ────────────────────────────────────────────────────────────────────

async def test_no_answer_requeues_and_counting_stays_exact(
    prd, fleet, pool, topology_factory
):
    topo = topology_factory(
        tag="na1",
        groups=[{"id": GROUP, "max_wait_secs": 120, "sla_target_secs": 30}],
        agents=[{"id": "1001", "groups": [GROUP], "max_concurrency": 1}],
        route_base=ROUTE_BASE,
        ring_timeout_secs=RING_TIMEOUT_SECS,
    )
    prd.drain(["1001"])
    (agent,) = await fleet.agents(["1001"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    base = agent.invite_count()

    caller = await place_call(prd, pool, topo)
    try:
        # ── 1. 进入振铃 = 出队 (the call leaves the waiting pool) ───────────
        ring = await agent.wait_ring(timeout=30)
        assert ring is not None, "agent 1001 never rang — dispatch failed"

        prd.wait_for(
            lambda: all(
                summary(s, "dispatching") == 1 and summary(s, "waiting") == 0
                for s in monitor(prd).values()
            ),
            timeout=8,
            what="ringing call counted as dispatching (not waiting) on both nodes",
        )

        rows = queue_rows(prd)
        assert len(rows) == 1, f"expected exactly 1 shared row while ringing, got {rows}"
        assert rows[0]["claimed_by"], (
            f"ringing call must hold a claim (振铃 = 出队): {rows[0]}"
        )
        assert agent.invite_count() - base == 1, "agent double-dialed during the first ring"

        # the session being dispatched (used later to prove the requeue reuses
        # the SAME root call)
        first_session = prd.wait_for(
            lambda: next((r["call_id"] for r in prd.call_node.ringing_agents()
                          if r["agent_id"] == "1001"), None),
            timeout=10, what="first-ring session id visible on call node",
        )

        # ── 2. 久振未接 → the call is RE-QUEUED and re-dispatched ───────────
        # The agent never answers. At the ring timeout the leg is cancelled and
        # the claim released; the customer must NOT be dropped and the SAME
        # call must come back (a 2nd INVITE) once the agent is free again.
        observed_requeue = False
        second_invite = False
        max_rows = 1
        deadline = asyncio.get_event_loop().time() + REQUEUE_OBSERVE_SECS
        last_log = 0.0
        while asyncio.get_event_loop().time() < deadline:
            rows = queue_rows(prd)
            max_rows = max(max_rows, len(rows))
            snaps = monitor(prd).values()
            now = asyncio.get_event_loop().time()
            if now - last_log >= 5:
                last_log = now
                print(
                    "[requeue-watch] "
                    f"waiting={[summary(s, 'waiting') for s in snaps]} "
                    f"dispatching={[summary(s, 'dispatching') for s in snaps]} "
                    f"status={ {n.name: n.agent_status('1001') for n in prd.nodes} } "
                    f"rows={len(rows)} invites={agent.invite_count() - base}"
                )
            if agent.invite_count() - base >= 2:
                second_invite = True
                break
            if all(summary(s, "waiting") >= 1 and summary(s, "dispatching") == 0
                   for s in snaps):
                observed_requeue = True
                break
            await asyncio.sleep(0.2)

        assert second_invite or observed_requeue, (
            "after the ring timeout the call was neither re-dispatched nor "
            f"returned to the waiting pool — 未接重新入队 broken; "
            f"rows={queue_rows(prd)} "
            f"monitor={ {k: v.get('summary') for k, v in monitor(prd).items()} }"
        )
        assert max_rows <= 1, (
            "a no-answer must not duplicate the queue row (one call = one row); "
            f"observed max_rows={max_rows}"
        )

        # ── 3. re-dispatch → connect ─────────────────────────────────────
        if not second_invite:
            force_idle(prd, "1001")
            await agent.wait_ring(timeout=45)

        # the re-dispatched call must be the SAME root call (requeue, not a
        # brand-new call) — the retry attempt carries a `-rN` suffix.
        disp_session = prd.wait_for(
            lambda: next((r["call_id"] for r in prd.call_node.ringing_agents()
                          if r["agent_id"] == "1001"), None),
            timeout=15, what="second-ring session id visible on call node",
        )
        root = first_session.split("-r")[0]
        assert disp_session.split("-r")[0] == root, (
            f"未接重新入队 must reuse the SAME call: first={first_session!r}, "
            f"redispatch={disp_session!r}"
        )

        assert await agent.answer_pending(), "agent should answer the re-dispatched call"
        prd.assert_agent_status_both_nodes("1001", "busy", timeout=15)

        prd.wait_for(
            lambda: all(
                summary(s, "waiting") == 0 and summary(s, "dispatching") == 0
                for s in monitor(prd).values()
            ),
            timeout=15,
            what="queue drain (waiting/dispatching == 0) after connect",
        )
        prd.wait_for(
            lambda: not queue_rows(prd),
            timeout=15,
            what="shared cc_acd_queue row removed after connect",
        )
        prd.call_node.end_all_active_calls()
        prd.drain(["1001"])
    finally:
        for node in prd.nodes:
            with suppress(PrdError):
                node.end_all_active_calls()
        with suppress(PrdError):
            prd.drain(["1001"])
        pool.terminate_all()
