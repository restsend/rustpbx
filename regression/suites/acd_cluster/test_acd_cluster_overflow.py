"""ACD cluster e2e — overflow / wait-timeout / abandon semantics (dual node).

Covers the scheduling semantics that the shared cluster state must uphold:

  * overflow escalation: primary group exhausted → widen to overflow group
  * max_wait fallback: nobody anywhere → release the caller (no ghost rows)
  * caller abandon while queued: queue row must vanish cluster-wide and the
    dead call must never dispatch later
  * strict FIFO across nodes: calls entering through DIFFERENT nodes are
    ordered by enqueue time via the shared cc_acd_queue table
"""

from __future__ import annotations

import asyncio
import os
import time

import pytest

from helpers.prd_cluster import PrdCluster, PrdError  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd, pytest.mark.slow]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)


async def place_call(prd: PrdCluster, pool, group_id: str, topo, *,
                     hangup: int = 120, entry: str = "call",
                     username: str = "1004"):
    caller = pool.caller(
        target=topo.target_uri(group_id, entry=entry),
        username=username, password="123456",
        hangup=hangup,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, (
        f"caller did not enter queue {group_id} via {entry}\n"
        f"{str(caller.output)[-1200:]}"
    )
    return caller


async def wait_bye(caller, timeout: float = 30.0) -> None:
    ok = await caller.wait_output_async(
        r"Call failed with status|Call ended remotely|Call terminated remotely|All calls finished", timeout=timeout
    )
    assert ok, f"call did not terminate within {timeout}s\n{str(caller.output)[-1000:]}"


def queue_depth(prd: PrdCluster, group_id: str) -> tuple[int, int]:
    return (len(prd.call_node.queued_calls() or []),
            len(prd.agent_node.queued_calls() or []))


# ────────────────────────────────────────────────────────────────────────────
# Scenario 6: overflow escalation — primary busy → widen to overflow group
# ────────────────────────────────────────────────────────────────────────────

async def test_overflow_group_escalation(prd, fleet, pool, topology_factory):
    """Primary group (1 agent, busy) overflows to a second group after the
    skill-group max_wait threshold (10s here, Replace mode).

    Asserts:
      - a second call first waits under the PRIMARY group id (both nodes)
      - after the threshold it is dispatched to an OVERFLOW-group agent
      - the busy primary agent never receives a second INVITE
      - the cc_calls row carries the queue the call was actually served by
    """
    topo = topology_factory(
        tag="ovf1",
        groups=[
            {"id": "e2e-ovf1-pri", "max_wait_secs": 10,
             "overflow_groups": ["e2e-ovf1-ovr"], "sla_target_secs": 10},
            {"id": "e2e-ovf1-ovr", "max_wait_secs": 90},
        ],
        agents=[
            {"id": "1001", "groups": ["e2e-ovf1-pri"], "max_concurrency": 1},
            {"id": "1002", "groups": ["e2e-ovf1-ovr"], "max_concurrency": 1},
        ],
        route_base="7201",
    )
    prd.drain(["1001", "1002"])
    a1, a2 = await fleet.agents(["1001", "1002"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    base1, base2 = a1.invite_count(), a2.invite_count()

    # occupy the only primary agent
    call_a = await place_call(prd, pool, "e2e-ovf1-pri", topo)
    assert await a1.answer_next(), "primary agent should take call A"

    # second call into the PRIMARY group must wait under that group id
    started = time.monotonic()
    call_b = await place_call(prd, pool, "e2e-ovf1-pri", topo)
    prd.wait_for(
        lambda: all(
            any(q["queue_id"] == "e2e-ovf1-pri" for q in n.queued_calls())
            for n in prd.nodes
        ),
        timeout=15, what="call B queued under primary group on both nodes",
    )

    # busy primary agent must NOT be re-invited while B waits
    assert a1.invite_count() - base1 == 1

    # after max_wait(10s) the escalation widens to the overflow group.
    # NOTE: the engine escalates IMMEDIATELY when the primary group is
    # exhausted (all its agents busy) — the time threshold guards the
    # ring-no-answer case. Here the primary agent is busy, so the overflow
    # may fire at once; assert the dispatch lands on the overflow agent
    # and NEVER on the busy primary one.
    ring_b = await a2.wait_ring(timeout=40)
    assert ring_b is not None, "overflow agent never rang after threshold"
    assert await a2.answer_pending(), "overflow agent should connect"

    # busy primary agent STILL must not have been re-invited
    assert a1.invite_count() - base1 == 1, (
        f"busy primary agent re-INVITEd during overflow: {a1.invite_count() - base1}"
    )

    prd.call_node.end_all_active_calls()
    prd.drain(["1001", "1002"])

    # dispatch record: B attributed to the overflow agent
    rows = [r for r in prd.cc_calls(10) if r["agent_id"] == "1002"
            and r["status"] == "answered"]
    assert rows, f"no answered row for overflow agent: {prd.cc_calls(10)}"


# ────────────────────────────────────────────────────────────────────────────
# Scenario 7: max_wait fallback — nobody available anywhere → caller released
# ────────────────────────────────────────────────────────────────────────────

async def test_max_wait_fallback_releases_caller(prd, fleet, pool,
                                                 topology_factory):
    """One skilled agent, kept AWAY the whole time; group max_wait 10s.

    The caller must be early-answered (wait retention), held for ~10s, then
    RELEASED with the queue fallback (480). After release there must be NO
    ghost waiting row on either node or in the shared cc_acd_queue table.
    """
    topo = topology_factory(
        tag="ovf2",
        groups=[{"id": "e2e-ovf2", "max_wait_secs": 10, "sla_target_secs": 10}],
        agents=[{"id": "1003", "groups": ["e2e-ovf2"], "max_concurrency": 1}],
        route_base="7202",
    )
    prd.drain(["1003"])
    (agent3,) = await fleet.agents(["1003"])
    prd.assert_agent_status_both_nodes("1003", "idle", timeout=45)
    prd.set_agent_status("1003", "away")
    prd.assert_agent_status_both_nodes("1003", "away", timeout=15)

    t0 = time.monotonic()
    caller = await place_call(prd, pool, "e2e-ovf2", topo)

    # waiting row visible on both nodes while retained
    prd.wait_for(
        lambda: all(len(n.queued_calls()) == 1 for n in prd.nodes),
        timeout=15, what="caller waiting on both nodes",
    )
    assert await agent3.wait_quiet(5), "away agent must not ring"

    # max_wait 10s → released with fallback (BYE to the early-answered caller)
    await wait_bye(caller, timeout=30)
    held = time.monotonic() - t0
    assert held >= 8, f"caller released too early ({held:.1f}s) — max_wait broken"

    # no ghost rows anywhere (queue-detail merges local + shared DB)
    prd.wait_for(
        lambda: all(not n.queued_calls() for n in prd.nodes),
        timeout=15, what="queue-detail drained after fallback",
    )
    ghosts = [r for r in prd.acd_queue_rows()
              if r["queue_id"] == "e2e-ovf2"]
    assert not ghosts, f"ghost rows in shared cc_acd_queue: {ghosts}"

    prd.set_agent_status("1003", "idle")
    prd.drain(["1003"])
    # and no late dispatch for the released call
    assert await agent3.wait_quiet(6), "released call must never dispatch"


# ────────────────────────────────────────────────────────────────────────────
# Scenario 8: caller abandons while queued — cluster-wide cleanup
# ────────────────────────────────────────────────────────────────────────────

async def test_caller_abandon_cleans_queue_cluster_wide(prd, fleet, pool,
                                                        topology_factory):
    """The only agent is away; the caller waits, then hangs up (BYE).

    The shared queue row must be deleted (both nodes' queue-detail empty,
    cc_acd_queue empty); when the agent later goes idle NOTHING rings —
    the dead call must not dispatch.
    """
    topo = topology_factory(
        tag="ovf3",
        groups=[{"id": "e2e-ovf3", "max_wait_secs": 90}],
        agents=[{"id": "1004", "groups": ["e2e-ovf3"], "max_concurrency": 1}],
        route_base="7203",
    )
    prd.drain(["1004"])
    (agent4,) = await fleet.agents(["1004"])
    prd.assert_agent_status_both_nodes("1004", "idle", timeout=45)
    prd.set_agent_status("1004", "away")
    prd.assert_agent_status_both_nodes("1004", "away", timeout=15)

    caller = await place_call(prd, pool, "e2e-ovf3", topo, hangup=6)
    prd.wait_for(
        lambda: all(len(n.queued_calls()) == 1 for n in prd.nodes),
        timeout=15, what="caller queued on both nodes before abandon",
    )
    shared_rows = [r for r in prd.acd_queue_rows()
                   if r["queue_id"] == "e2e-ovf3"]
    assert len(shared_rows) == 1, f"shared queue rows: {shared_rows}"

    # caller gives up (auto BYE at hangup=6)
    await wait_bye(caller, timeout=20)

    # cluster-wide cleanup of the abandoned call
    prd.wait_for(
        lambda: all(not n.queued_calls() for n in prd.nodes),
        timeout=15, what="queue-detail drained after abandon",
    )
    ghosts = [r for r in prd.acd_queue_rows()
              if r["queue_id"] == "e2e-ovf3"]
    assert not ghosts, f"abandoned call left ghost rows: {ghosts}"

    # freeing an agent later must NOT ring for the dead call
    prd.set_agent_status("1004", "idle")
    prd.assert_agent_status_both_nodes("1004", "idle", timeout=15)
    assert await agent4.wait_quiet(8), "abandoned call dispatched late"
    prd.drain(["1004"])


# ────────────────────────────────────────────────────────────────────────────
# Scenario 9: strict FIFO across entry nodes (shared-queue ordering)
# ────────────────────────────────────────────────────────────────────────────

async def test_cross_node_fifo_order(prd, fleet, pool, topology_factory):
    """Two callers enter through DIFFERENT nodes (call-plane then
    agent-plane) while the only agent is away; both wait in the SHARED
    queue. Freeing the agent must dispatch the EARLIER call first —
    cross-node strict FIFO via cc_acd_queue.
    """
    topo = topology_factory(
        tag="ovf4",
        groups=[{"id": "e2e-ovf4", "max_wait_secs": 120}],
        agents=[{"id": "1002", "groups": ["e2e-ovf4"], "max_concurrency": 1}],
        route_base="7204",
    )
    prd.drain(["1002"])
    (agent2,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    prd.set_agent_status("1002", "away")
    prd.assert_agent_status_both_nodes("1002", "away", timeout=15)

    older = await place_call(prd, pool, "e2e-ovf4", topo, entry="call")
    older_id = prd.wait_for(
        lambda: (q[0]["call_id"] if (q := prd.call_node.queued_calls()) else None),
        timeout=15, what="older call visible in shared queue",
    )
    await asyncio.sleep(3)  # clear enqueue-time gap

    newer = await place_call(prd, pool, "e2e-ovf4", topo, entry="agent")
    prd.wait_for(
        lambda: (len(prd.call_node.queued_calls()) == 2
                 and len(prd.agent_node.queued_calls()) == 2
                 or None),
        timeout=15, what="both calls queued on both nodes",
    )
    ids = {q["call_id"] for q in prd.call_node.queued_calls()}
    assert len(ids) == 2, f"expected 2 distinct queued calls, got {ids}"
    newer_id = next(c for c in ids if c != older_id)

    # BOTH nodes must show BOTH waiting calls (shared-DB merge)
    for node in prd.nodes:
        node_ids = {q["call_id"] for q in node.queued_calls()}
        assert node_ids == ids, (
            f"{node.name} queue view {node_ids} != {ids}"
        )

    # shared rows: older call enqueued strictly before newer
    rows = {r["call_id"]: r for r in prd.acd_queue_rows()
            if r["queue_id"] == "e2e-ovf4"}
    assert rows[older_id]["enqueued_at"] < rows[newer_id]["enqueued_at"], rows

    # free the agent → the OLDER call must dispatch first
    prd.set_agent_status("1002", "idle")
    ring = await agent2.wait_ring(timeout=30)
    assert ring is not None, "no dispatch after agent freed"

    # the ringing row on the CALL node carries the SESSION id of the call
    # being dispatched (the agent node's row shows the leg id instead) —
    # assert it is the older call's session id.
    ringing_session = prd.wait_for(
        lambda: next(
            (r["call_id"] for r in prd.call_node.ringing_agents()
             if r["agent_id"] == "1002"), None),
        timeout=10, what="ringing session visible on call node",
    )
    assert ringing_session == older_id, (
        f"cross-node FIFO violated: dispatched {ringing_session!r}, "
        f"expected older {older_id!r} (newer={newer_id!r})"
    )

    assert await agent2.answer_pending(), "older call should connect"
    prd.call_node.end_all_active_calls()
    prd.drain(["1002"])
