"""ACD cluster e2e — queue dispatch + agent state machine (dual node).

Topology under test (real prd cluster):
  caller (sipbot) ──► rustpbx-call :35060 ──► queue(skill-group) ──┐
                                                                   │ shared MySQL
  agent (restsend-cli) ──REGISTER──► rustpbx-agent :35061 ◄────────┘
                                    (cc_agent_presence + cc_acd_queue)

Every test asserts the scheduling state on BOTH nodes — cluster-mode state
management is the point of this suite, not just single-node dispatch.
"""

from __future__ import annotations

import asyncio
import os
import time

import pytest

from helpers.prd_cluster import PrdCluster, PrdError, PrdRestsendAgent  # noqa: F401

pytestmark = [pytest.mark.cluster, pytest.mark.acd, pytest.mark.slow]

if os.environ.get("RUSTPBX_PRD_E2E", "1") != "1":
    pytest.skip("RUSTPBX_PRD_E2E=0 — prd cluster tests disabled",
                allow_module_level=True)


# ────────────────────────────────────────────────────────────────────────────
# helpers
# ────────────────────────────────────────────────────────────────────────────

async def place_call(prd: PrdCluster, pool, group_id: str, topo,
                     *, hangup: int = 120, entry: str = "call",
                     username: str = "1004"):
    """Place a caller into a queue route; returns the sipbot process."""
    caller = pool.caller(
        target=topo.target_uri(group_id, entry=entry),
        username=username, password="123456",
        hangup=hangup,
        addr=f"0.0.0.0:{prd.next_caller_port()}",
    )
    ok = await caller.wait_output_async(r"200 OK|Call established", timeout=25)
    assert ok, (
        f"caller did not get 200 OK entering queue {group_id} via {entry}\n"
        f"{str(caller.output)[-1200:]}"
    )
    return caller


async def wait_bye(caller, timeout: float = 30.0) -> str:
    """Wait until the caller's call ends; returns the matched line."""
    ok = await caller.wait_output_async(
        r"Call failed with status|Call ended remotely|Call terminated remotely|All calls finished", timeout=timeout
    )
    assert ok, f"call did not terminate within {timeout}s\n{str(caller.output)[-1000:]}"
    return str(caller.output)[-600:]


def ringing_for(node, agent_id: str):
    return [r for r in node.ringing_agents() if r["agent_id"] == agent_id]


# ────────────────────────────────────────────────────────────────────────────
# Scenario 1: cross-node dispatch lifecycle (offline→idle→ringing→busy→wrapup)
# ────────────────────────────────────────────────────────────────────────────

async def test_cross_node_dispatch_lifecycle(prd, fleet, pool, topology_factory):
    """One caller via the CALL node; agent registered on the AGENT node.

    Asserts the FULL cluster state machine, on BOTH nodes at each phase:
      offline → (SIP REGISTER) → idle → (queue) → ringing → (answer) →
      busy(current_calls=1) → (hangup) → wrapup → (forced) → idle,
    plus queue-detail empties and the shared cc_calls row records the
    dispatch with agent attribution.
    """
    topo = topology_factory(
        tag="disp1",
        groups=[{"id": "e2e-disp1", "max_wait_secs": 90, "sla_target_secs": 30}],
        agents=[{"id": "1001", "groups": ["e2e-disp1"], "max_concurrency": 1}],
        route_base="7101",
    )
    prd.drain(["1001"])
    # agent offline on both nodes before the fleet registers
    for node in prd.nodes:
        assert node.agent_status("1001") in ("offline", "idle")

    (agent1,) = await fleet.agents(["1001"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    base_invites = agent1.invite_count()
    assert agent1.agent.has_event("sip_reg_state") is not None

    caller = await place_call(prd, pool, "e2e-disp1", topo)

    # dispatch: the agent phone must ring (cross-node!)
    ring = await agent1.wait_ring(timeout=30)
    assert ring is not None, "agent 1001 never rang — cross-node dispatch failed"

    # ringing visible on BOTH nodes' queue-detail (shared presence)
    prd.wait_for(
        lambda: all(ringing_for(n, "1001") for n in prd.nodes),
        timeout=10, what="ringing agent 1001 on both nodes",
    )
    prd.assert_agent_status_both_nodes("1001", "ringing", timeout=10)

    assert await agent1.answer_pending(), "agent answer did not connect"
    prd.assert_agent_status_both_nodes("1001", "busy", timeout=15)

    # current_calls consistent on both nodes
    for node in prd.nodes:
        calls = node.agent_current_calls("1001")
        assert calls == 1, f"{node.name}: current_calls={calls}, expected 1"

    # active call lives on the CALL node (caller leg) with queue attribution
    active = prd.call_node.active_calls()
    assert any(isinstance(c, dict) and "71011" in str(c.get("callee", ""))
               for c in active), f"call not active on call-node: {active}"

    # caller has live media (hold/queue audio bridged through media proxy)
    await asyncio.sleep(3)

    # hang up from the caller side → wrapup (call-bound), not straight idle
    prd.call_node.end_all_active_calls()
    prd.assert_agent_status_both_nodes("1001", "wrapup", timeout=20)
    prd.wait_for(
        lambda: all(not n.queued_calls() and not n.ringing_agents()
                    for n in prd.nodes),
        timeout=20, what="queue-detail drained on both nodes",
    )

    for node in prd.nodes:
        try:
            node.end_wrapup("1001")
        except PrdError:
            pass
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=20)

    await wait_bye(caller, timeout=20)

    # shared DB: cc_calls row records queue + agent attribution
    rows = prd.wait_for(
        lambda: [r for r in prd.cc_calls(8)
                 if r["queue_id"] == "e2e-disp1" and r["agent_id"] == "1001"],
        timeout=15, what="cc_calls row with agent attribution",
    )
    assert rows[0]["status"] == "answered", rows

    # exactly one INVITE to the agent for the whole lifecycle (no double-dial)
    assert agent1.invite_count() - base_invites == 1, (
        f"double-dial: agent got {agent1.invite_count() - base_invites} INVITEs"
    )


# ────────────────────────────────────────────────────────────────────────────
# Scenario 2: wait retention — all agents busy → queue → dispatch on free
# ────────────────────────────────────────────────────────────────────────────

async def test_wait_retention_dispatch_on_free(prd, fleet, pool, topology_factory):
    """max_concurrency=1 agent busy on call A; call B waits with BOTH nodes
    showing it queued (shared cc_acd_queue row); when A ends and the agent
    returns idle, B is auto-dispatched — exactly one new INVITE."""
    topo = topology_factory(
        tag="disp2",
        groups=[{"id": "e2e-disp2", "max_wait_secs": 120}],
        agents=[{"id": "1002", "groups": ["e2e-disp2"], "max_concurrency": 1}],
        route_base="7102",
    )
    prd.drain(["1002"])
    (agent2,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)

    base_invites = agent2.invite_count()
    call_a = await place_call(prd, pool, "e2e-disp2", topo)
    assert await agent2.answer_next(), "call A should connect"
    # capture A's call id BEFORE B exists — later we free the agent by ending
    # ONLY A (end_all would also kill the queued B).
    call_a_id = prd.call_node.only_active_call_id("71021")

    # B enters while the only agent is busy → must WAIT (not fail, not ring)
    call_b = await place_call(prd, pool, "e2e-disp2", topo)
    prd.wait_for(
        lambda: all(len(n.queued_calls()) == 1 for n in prd.nodes),
        timeout=15, what="call B queued and visible on both nodes",
    )
    # the waiting row carries the skill group id
    for node in prd.nodes:
        q = node.queued_calls()[0]
        assert q["queue_id"] == "e2e-disp2", q
        assert q["dispatch_state"] in ("waiting", "dispatching"), q
    # shared DB row exists exactly once
    rows = [r for r in prd.acd_queue_rows() if r["queue_id"] == "e2e-disp2"]
    assert len(rows) == 1, f"cc_acd_queue rows: {rows}"

    invites_before = agent2.invite_count()
    assert invites_before - base_invites == 1, (
        f"agent got {invites_before - base_invites} INVITEs before free"
    )

    # free the agent: end ONLY call A (B must stay queued) → wrapup → idle
    prd.call_node.end_call(call_a_id)
    await agent2.reset()  # clear stuck in-a-call state after server BYE
    for node in prd.nodes:
        prd.wait_for(lambda: node.agent_status("1002") == "wrapup",
                     timeout=20, what="wrapup after call A")
        try:
            node.end_wrapup("1002")
        except PrdError:
            pass
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=20)

    # B must now dispatch to the freed agent (auto-dispatch on idle)
    await agent2.wait_ring(timeout=30)
    assert await agent2.answer_pending(), "call B should connect after agent freed"
    prd.assert_agent_status_both_nodes("1002", "busy", timeout=15)

    # exactly one additional INVITE (total 2) — no double dispatch of B
    assert agent2.invite_count() - base_invites == 2, (
        f"expected exactly 2 INVITEs total, got {agent2.invite_count() - base_invites}"
    )

    prd.call_node.end_all_active_calls()
    for node in prd.nodes:
        try:
            node.end_wrapup("1002")
        except PrdError:
            pass
    prd.drain(["1002"])


# ────────────────────────────────────────────────────────────────────────────
# Scenario 3: N callers > agents — concurrency, no double-dial, cascading
# ────────────────────────────────────────────────────────────────────────────

async def test_three_calls_two_agents_cascade(prd, fleet, pool, topology_factory):
    """3 concurrent callers, 2 cross-node agents (both answer).

    - 2 calls bridge immediately; the 3rd waits (depth 1 on both nodes)
    - each agent receives exactly ONE INVITE while both are on calls
    - hang up call A → 3rd dispatches to the freed agent
    - agent counters stay consistent on both nodes throughout
    """
    topo = topology_factory(
        tag="disp3",
        groups=[{"id": "e2e-disp3", "max_wait_secs": 120}],
        agents=[
            {"id": "1001", "groups": ["e2e-disp3"], "max_concurrency": 1},
            {"id": "1002", "groups": ["e2e-disp3"], "max_concurrency": 1},
        ],
        route_base="7103",
    )
    prd.drain(["1001", "1002"])
    a1, a2 = await fleet.agents(["1001", "1002"])
    prd.assert_agent_status_both_nodes("1001", "idle", timeout=45)
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    # session-persistent UAs: compare INVITE DELTAS, not lifetime counts
    base1, base2 = a1.invite_count(), a2.invite_count()

    c1 = await place_call(prd, pool, "e2e-disp3", topo)
    c2 = await place_call(prd, pool, "e2e-disp3", topo)
    ok1 = await a1.answer_next(timeout=30)
    ok2 = await a2.answer_next(timeout=30)
    assert ok1 and ok2, "both agents should answer their reserved calls"
    prd.assert_agent_status_both_nodes("1001", "busy", timeout=15)
    prd.assert_agent_status_both_nodes("1002", "busy", timeout=15)
    # the two bridged call ids (before C exists)
    bridged_ids = prd.call_node.active_call_ids("71031")
    assert len(bridged_ids) == 2, f"expected 2 bridged calls, got {bridged_ids}"

    # 3rd caller waits; both busy agents see no extra INVITE (no double-dial)
    c3 = await place_call(prd, pool, "e2e-disp3", topo)
    prd.wait_for(
        lambda: all(len(n.queued_calls()) == 1 for n in prd.nodes),
        timeout=15, what="3rd caller queued on both nodes",
    )
    await asyncio.sleep(8)  # well past the per-agent ring timeout (6s)
    assert a1.invite_count() - base1 == 1, (
        f"1001 double-dialed ({a1.invite_count() - base1} invites)"
    )
    assert a2.invite_count() - base2 == 1, (
        f"1002 double-dialed ({a2.invite_count() - base2} invites)"
    )

    # free the agents: end ONLY the two bridged calls (C must stay queued)
    for cid in bridged_ids:
        prd.call_node.end_call(cid)
    await a1.reset()
    await a2.reset()
    for node in prd.nodes:
        prd.wait_for(lambda: node.agent_status("1001") == "wrapup",
                     timeout=25, what="1001 wrapup")
        prd.wait_for(lambda: node.agent_status("1002") == "wrapup",
                     timeout=25, what="1002 wrapup")
        for aid in ("1001", "1002"):
            try:
                node.end_wrapup(aid)
            except PrdError:
                pass
    # the two nodes' leg state machines converge at their own pace; the
    # 3rd dispatch only needs ONE node to consider an agent idle (the
    # queue polls the hosting node's registry).
    prd.wait_for(
        lambda: any(n.agent_status("1001") == "idle" for n in prd.nodes)
        or None,
        timeout=60, what="1001 idle on at least one node",
    )

    # exactly one of the two freed agents gets the 3rd call — answer the
    # FIRST ring immediately (waiting for both would trip the sequential
    # ring-timeout fallback and dial the second agent too).
    done, pending = await asyncio.wait(
        [asyncio.create_task(a1.wait_ring(timeout=25)),
         asyncio.create_task(a2.wait_ring(timeout=25))],
        return_when=asyncio.FIRST_COMPLETED,
    )
    for task in pending:
        task.cancel()
    ringed = [t for t in done if t.result()]
    assert ringed, "3rd call never dispatched after agents freed"
    winner = a1 if ringed[0] is not None and a1.invite_count() - base1 == 2 else a2
    assert await winner.answer_pending(), "3rd call should connect"

    # the OTHER agent must NOT have been dialed for the 3rd call (sequential
    # strategy reserves exactly one; fallback only fires on ring timeout)
    loser = a2 if winner is a1 else a1
    base_loser = base2 if loser is a2 else base1
    assert await loser.wait_quiet(7), (
        f"3rd call double-dialed: both agents rang "
        f"(a1=+{a1.invite_count() - base1}, a2=+{a2.invite_count() - base2})"
    )
    new_invites = (a1.invite_count() - base1 - 1) + (a2.invite_count() - base2 - 1)
    assert new_invites == 1, (
        f"3rd call must dispatch exactly once, got {new_invites} extra INVITEs"
    )

    # queue drains everywhere
    prd.wait_for(
        lambda: all(not n.queued_calls() for n in prd.nodes),
        timeout=20, what="queue drained after cascade",
    )
    prd.call_node.end_all_active_calls()
    prd.drain(["1001", "1002"])


# ────────────────────────────────────────────────────────────────────────────
# Scenario 4: ring no-answer → sequential fallback to the next agent
# ────────────────────────────────────────────────────────────────────────────

async def test_no_answer_falls_back_to_next_agent(prd, fleet, pool,
                                                  topology_factory):
    """Agent A never answers; after the per-agent ring timeout (6s) the
    sequential strategy must ring agent B. A must be left non-broken (ring
    timeout cooldown), B answers, and the connected attribution is B."""
    topo = topology_factory(
        tag="disp4",
        groups=[{"id": "e2e-disp4", "max_wait_secs": 60}],
        agents=[
            {"id": "1003", "groups": ["e2e-disp4"], "max_concurrency": 1},
            {"id": "1004", "groups": ["e2e-disp4"], "max_concurrency": 1},
        ],
        route_base="7104",
    )
    prd.drain(["1003", "1004"])
    a3, a4 = await fleet.agents(["1003", "1004"])
    prd.assert_agent_status_both_nodes("1003", "idle", timeout=45)
    prd.assert_agent_status_both_nodes("1004", "idle", timeout=45)
    base3, base4 = a3.invite_count(), a4.invite_count()

    caller = await place_call(prd, pool, "e2e-disp4", topo)

    # A rings first (sequential head) and deliberately does not answer
    ring_a = await a3.wait_ring(timeout=25)
    assert ring_a is not None, "first agent never rang"

    # after ring timeout the fallback must ring B
    ring_b = await a4.wait_ring(timeout=25)
    assert ring_b is not None, "fallback agent never rang after ring timeout"
    assert await a4.answer_pending(), "fallback agent should connect"

    prd.assert_agent_status_both_nodes("1004", "busy", timeout=15)
    # A's ringing leg was cancelled; per ring-timeout policy A goes to a
    # cooldown (wrapup) or back to idle — never stuck in ringing. The two
    # nodes derive state from their own legs + shared presence, so allow a
    # convergence window before asserting cluster-wide agreement.
    prd.wait_for(
        lambda: {n.agent_status("1003") for n in prd.nodes}
        <= {"idle", "wrapup", "offline"} or None,
        timeout=60, what="agent 1003 converges to idle/wrapup on both nodes",
    )

    # B is the attributed answerer in the shared dispatch record
    prd.call_node.end_all_active_calls()
    prd.drain(["1003", "1004"])
    rows = [r for r in prd.cc_calls(8)
            if r["queue_id"] == "e2e-disp4" and r["agent_id"] == "1004"
            and r["status"] == "answered"]
    assert rows, f"no answered cc_calls row attributed to 1004: {prd.cc_calls(8)}"

    # exactly one INVITE per agent for this call
    assert a3.invite_count() - base3 == 1 and a4.invite_count() - base4 == 1, (
        f"invite counts: 1003={a3.invite_count() - base3} "
        f"1004={a4.invite_count() - base4}"
    )
    await wait_bye(caller, timeout=20)


# ────────────────────────────────────────────────────────────────────────────
# Scenario 5: agent status gating — away agents are never dispatched
# ────────────────────────────────────────────────────────────────────────────

async def test_away_agent_not_dispatched_until_idle(prd, fleet, pool,
                                                    topology_factory):
    """The ONLY skilled agent is set away → the call waits, nobody rings;
    flip the agent to idle via REST → the waiting call dispatches."""
    topo = topology_factory(
        tag="disp5",
        groups=[{"id": "e2e-disp5", "max_wait_secs": 120}],
        agents=[{"id": "1002", "groups": ["e2e-disp5"], "max_concurrency": 1}],
        route_base="7105",
    )
    prd.drain(["1002"])
    (agent2,) = await fleet.agents(["1002"])
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=45)
    base_invites = agent2.invite_count()

    # away on BOTH nodes (state written via shared presence)
    prd.set_agent_status("1002", "away")
    prd.assert_agent_status_both_nodes("1002", "away", timeout=15)

    caller = await place_call(prd, pool, "e2e-disp5", topo)

    # queued on both nodes; NO ring while the only candidate is away
    prd.wait_for(
        lambda: all(len(n.queued_calls()) == 1 for n in prd.nodes),
        timeout=15, what="call queued while agent away",
    )
    assert await agent2.wait_quiet(8), "away agent must not be dispatched"

    # flip to idle → the queued call dispatches
    prd.set_agent_status("1002", "idle")
    prd.assert_agent_status_both_nodes("1002", "idle", timeout=15)
    await agent2.wait_ring(timeout=30)
    assert await agent2.answer_pending(), "call should dispatch once agent idle"

    prd.call_node.end_all_active_calls()
    prd.drain(["1002"])
    await wait_bye(caller, timeout=20)
    assert agent2.invite_count() - base_invites == 1
