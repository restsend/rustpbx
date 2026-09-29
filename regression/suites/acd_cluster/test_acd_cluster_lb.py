"""ACD cluster e2e — entry through the siprouted edge load balancer.

siprouted (:15060) balances across the two rustpbx nodes (35060 w5 /
35061 w1). Dispatch invariants must hold whichever node takes the INVITE,
and both nodes' queue views must agree (shared-DB state management).
"""

from __future__ import annotations

import asyncio
import os

import pytest

from helpers.prd_cluster import PrdCluster, PrdError  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)


async def place_edge_call(prd: PrdCluster, pool, group_id: str, topo, *,
                          hangup: int = 120):
    caller = pool.caller(
        target=topo.target_uri(group_id, entry="edge"),
        username="1004", password="123456",
        hangup=hangup,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, (
        f"caller via siprouted edge did not get 200 OK\n"
        f"{str(caller.output)[-1200:]}"
    )
    return caller


async def test_dispatch_via_siprouted_edge(prd, fleet, pool, topology_factory):
    """Call enters via the edge LB; an agent registered on the AGENT node
    answers. Both nodes' queue-detail must reflect the full lifecycle no
    matter which node hosted the caller leg."""
    topo = topology_factory(
        tag="lb1",
        groups=[{"id": "e2e-lb1", "max_wait_secs": 90}],
        agents=[{"id": "1001", "groups": ["e2e-lb1"], "max_concurrency": 1}],
        route_base="7301",
    )
    prd.drain(["1001"])
    (agent1,) = await fleet.agents(["1001"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    base_invites = agent1.invite_count()

    caller = await place_edge_call(prd, pool, "e2e-lb1", topo)

    ring = await agent1.wait_ring(timeout=30)
    assert ring is not None, "edge-LB call never dispatched to the agent"
    # ringing rows are derived from each node's LOCAL registry; the leg
    # passes through whichever node hosts the edge-routed call, so at
    # least ONE node must show the ringing agent.
    prd.wait_for(
        lambda: any(any(r["agent_id"] == "1001" for r in n.ringing_agents())
                    for n in prd.nodes) or None,
        timeout=15, what="ringing visible for edge call",
    )
    assert await agent1.answer_pending(), "edge-LB call should connect"
    prd.assert_agent_status_both_nodes("1001", "busy", timeout=15)

    prd.call_node.end_all_active_calls()
    prd.drain(["1001"])
    ok = await caller.wait_output_async(
        r"Call failed with status|Call ended remotely|Call terminated remotely|All calls finished", timeout=25
    )
    assert ok, f"edge call not released\n{str(caller.output)[-800:]}"
    assert agent1.invite_count() - base_invites == 1


async def test_edge_and_direct_entries_coexist(prd, fleet, pool,
                                               topology_factory):
    """One call via the edge LB + one direct to the agent node, same queue,
    single agent: the second must WAIT (visible on both nodes) and dispatch
    after the first ends."""
    topo = topology_factory(
        tag="lb2",
        groups=[{"id": "e2e-lb2", "max_wait_secs": 120}],
        agents=[{"id": "1002", "groups": ["e2e-lb2"], "max_concurrency": 1}],
        route_base="7302",
    )
    prd.drain(["1002"])
    (agent2,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    base_invites = agent2.invite_count()

    first = await place_edge_call(prd, pool, "e2e-lb2", topo)
    assert await agent2.answer_next(), "first (edge) call should connect"
    # the edge LB may host the call on either node — end it via the OWNING
    # node (end_call on a peer that does not host the call is a no-op).
    first_ids_call = prd.call_node.active_call_ids("73021")
    first_ids_agent = prd.agent_node.active_call_ids("73021")
    assert len(first_ids_call) + len(first_ids_agent) == 1, (
        f"expected exactly 1 bridged call, got {first_ids_call + first_ids_agent}"
    )
    owner = prd.call_node if first_ids_call else prd.agent_node
    first_id = (first_ids_call or first_ids_agent)[0]

    second = pool.caller(
        target=topo.target_uri("e2e-lb2", entry="agent"),
        username="1004", password="123456", hangup=120,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await second.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, f"direct entry call failed\n{str(second.output)[-800:]}"

    prd.wait_for(
        lambda: all(len(n.queued_calls()) == 1 for n in prd.nodes),
        timeout=15, what="second call queued on both nodes",
    )

    # free the agent by ending ONLY the first call (second stays queued)
    owner.end_call(first_id)
    await agent2.reset()
    for node in prd.nodes:
        prd.wait_for(lambda: node.agent_status("1002") == "wrapup",
                     timeout=25, what="wrapup after first call")
        try:
            node.end_wrapup("1002")
        except PrdError:
            pass
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=20)

    await agent2.wait_ring(timeout=30)
    assert await agent2.answer_pending(), "second call should dispatch"
    assert agent2.invite_count() - base_invites == 2

    prd.call_node.end_all_active_calls()
    prd.drain(["1002"])
